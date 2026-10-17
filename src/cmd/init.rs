use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use crate::config::{Config, Destination, Format, RetryConfig, Source, StreamConfig};
use crate::detect::{self, AuthOption, Detection};
use crate::envfile;
use crate::prompt::{self, Prompter};
use crate::spec::Spec;
use crate::workspace::Workspace;

use super::{pagination_label, print_rate_limit_warning};

#[derive(Debug, Default, Clone)]
pub struct InitOptions {
    pub spec: String,
    pub force: bool,
    pub yes: bool,
    pub destination: Option<String>,
    pub format: Option<String>,
    pub auth: Option<String>,
    pub plan: Option<String>,
    pub streams: Option<Vec<String>>,
    pub base_url: Option<String>,
    pub s3_endpoint: Option<String>,
    pub s3_region: Option<String>,
}

pub async fn run(ws: &Workspace, o: InitOptions) -> Result<()> {
    if ws.has_config() && !o.force {
        bail!(
            "{} already exists. Use `apisip update` to follow spec changes, or `apisip init --force` to start over.",
            ws.config_path().display()
        );
    }
    let p = Prompter::new(o.yes)?;
    let cwd = std::env::current_dir()?;
    let spec = Spec::load(&o.spec, &cwd).await?;
    let det = detect::detect(&spec);
    eprintln!(
        "Read \"{}\": {} data endpoint(s) found, {} skipped.",
        det.title,
        det.streams.len(),
        det.skipped.len()
    );
    if det.streams.is_empty() {
        bail!("no GET endpoint returning a list was found in this spec");
    }

    let base_url = match o.base_url.clone().or_else(|| det.base_url.clone()) {
        Some(u) => p.text("API base URL", &u, None)?,
        None if p.interactive => p.text("API base URL (the spec declares no server)", "", None)?,
        None => bail!("the spec declares no server URL: pass --base-url"),
    };
    if base_url.is_empty() {
        bail!("a base URL is required");
    }

    // 1. Destination.
    let mut env_updates: Vec<(String, Option<String>)> = Vec::new();
    let dest_in = p.text(
        "Where should apisip write data?",
        o.destination.as_deref().unwrap_or("data"),
        Some("a directory (default: data/ in this folder) or s3://bucket/prefix"),
    )?;
    let mut destination = Destination {
        url: normalize_destination(&dest_in),
        format: Format::Jsonl,
        rotate: Default::default(),
        endpoint: None,
        region: None,
    };
    if destination.url.starts_with("s3://") {
        let endpoint_default = o
            .s3_endpoint
            .clone()
            .or_else(|| std::env::var("AWS_ENDPOINT_URL").ok())
            .unwrap_or_default();
        let endpoint = p.text(
            "S3 endpoint URL",
            &endpoint_default,
            Some("leave empty for AWS; set it for MinIO, R2, Ceph..."),
        )?;
        destination.endpoint = (!endpoint.is_empty()).then_some(endpoint);
        let region_default = o
            .s3_region
            .clone()
            .or_else(|| std::env::var("AWS_REGION").ok())
            .unwrap_or_else(|| "us-east-1".into());
        destination.region = Some(p.text("S3 region", &region_default, None)?);
        let s3_vars = [
            detect::EnvVar {
                name: "AWS_ACCESS_KEY_ID".into(),
                label: "S3 access key id".into(),
                secret: false,
            },
            detect::EnvVar {
                name: "AWS_SECRET_ACCESS_KEY".into(),
                label: "S3 secret access key".into(),
                secret: true,
            },
        ];
        env_updates.extend(prompt::ask_credentials(&p, &s3_vars, |k| {
            current_value(ws, k)
        })?);
    }

    // 2. Format.
    let formats = vec!["jsonl".to_string(), "parquet".to_string()];
    let default_format = match o.format.as_deref() {
        Some("parquet") => 1,
        _ => 0,
    };
    destination.format = match p.select("Output format", &formats, Some(default_format))? {
        1 => Format::Parquet,
        _ => Format::Jsonl,
    };

    // 3 and 4. Auth scheme and credentials.
    let (auth, auth_vars, creds) = pick_auth(&p, ws, &det, o.auth.as_deref())?;
    env_updates.extend(creds);

    // 5. Streams.
    let mut streams: Vec<StreamConfig> = det.streams.iter().map(|s| s.config.clone()).collect();
    let labels: Vec<String> = det
        .streams
        .iter()
        .map(|s| {
            format!(
                "{:<24} GET {:<32} {}, {}",
                s.config.name,
                s.config.path,
                s.config.mode().label(),
                pagination_label(&s.config.pagination)
            )
        })
        .collect();
    let defaults: Vec<usize> = match &o.streams {
        Some(names) => {
            for n in names {
                if !streams.iter().any(|s| &s.name == n) {
                    bail!(
                        "--streams: no stream named `{n}` (detected: {})",
                        stream_names(&streams)
                    );
                }
            }
            (0..streams.len())
                .filter(|i| names.contains(&streams[*i].name))
                .collect()
        }
        None => (0..streams.len()).collect(),
    };
    let picked = p.multi_select("Streams to download", &labels, &defaults)?;
    for (i, s) in streams.iter_mut().enumerate() {
        s.enabled = picked.contains(&i);
    }

    // 6. Primary keys, only where the record schema has no `id`.
    for (s, d) in streams.iter_mut().zip(&det.streams) {
        if !s.enabled || !s.primary_key.is_empty() || d.fields.is_empty() || !p.interactive {
            continue;
        }
        let mut options = d.fields.clone();
        options.push("(no primary key)".into());
        let i = p.select(
            &format!("Primary key of `{}` (records have no `id` field)", s.name),
            &options,
            Some(
                d.fields
                    .iter()
                    .position(|f| f.ends_with("_id") || f.ends_with("Id"))
                    .unwrap_or(0),
            ),
        )?;
        if i < d.fields.len() {
            s.primary_key = vec![d.fields[i].clone()];
        }
    }

    // 7. Rate-limit plan, only when the spec documents several.
    let plans = det.rate_limits.plans();
    let rate_limit = if plans.is_empty() {
        det.rate_limits.default_limit()
    } else {
        let names: Vec<String> = plans
            .iter()
            .map(|t| {
                format!(
                    "{} ({} calls per {})",
                    t.plan.as_deref().unwrap_or("?"),
                    t.limit.max_calls,
                    humantime::format_duration(t.limit.period)
                )
            })
            .collect();
        let default = match &o.plan {
            Some(want) => Some(
                plans
                    .iter()
                    .position(|t| {
                        t.plan
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(want))
                    })
                    .with_context(|| {
                        format!(
                            "--plan {want}: the spec documents {}",
                            plans
                                .iter()
                                .filter_map(|t| t.plan.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?,
            ),
            None => Some(0),
        };
        let i = p.select(
            "Rate limits depend on the plan. Which plan are you on?",
            &names,
            default,
        )?;
        Some(plans[i].limit.clone())
    };

    let config = Config {
        source: Source {
            spec: o.spec.clone(),
            base_url: base_url.clone(),
            auth: auth.clone(),
            rate_limit: rate_limit.clone(),
            retry: RetryConfig::default(),
            concurrency: 4,
            headers: BTreeMap::new(),
        },
        destination,
        streams,
    };
    config.validate()?;

    let baseline = baseline(&det, &config.source);
    write_files(
        ws,
        &spec,
        &det,
        &config,
        &baseline,
        &env_updates,
        &auth_vars,
    )?;
    print_summary(ws, &det, &config, &auth_vars, &env_updates);
    Ok(())
}

/// What detection alone produced, with the auth and source settings of the
/// config: `update` treats every difference between it and apisip.toml as a
/// user edit to preserve.
pub(crate) fn baseline(det: &Detection, source: &Source) -> Config {
    Config {
        source: Source {
            base_url: det
                .base_url
                .clone()
                .unwrap_or_else(|| source.base_url.clone()),
            // A plan picked at init is the user's choice, not detection's.
            rate_limit: if det.rate_limits.plans().is_empty() {
                det.rate_limits.default_limit()
            } else {
                None
            },
            ..source.clone()
        },
        destination: Destination {
            url: crate::config::DEFAULT_DESTINATION.into(),
            format: Format::Jsonl,
            rotate: Default::default(),
            endpoint: None,
            region: None,
        },
        streams: det.streams.iter().map(|s| s.config.clone()).collect(),
    }
}

pub(crate) const BASELINE_HEADER: &str =
    "# Detected by apisip from the spec. Do not edit: apisip update compares against it.";

type AuthPick = (
    Option<crate::config::AuthConfig>,
    Vec<detect::EnvVar>,
    Vec<(String, Option<String>)>,
);

fn pick_auth(
    p: &Prompter,
    ws: &Workspace,
    det: &Detection,
    flag: Option<&str>,
) -> Result<AuthPick> {
    let declared: Vec<AuthOption> = det.auth.clone();
    for o in declared.iter().filter(|o| o.config.is_none()) {
        eprintln!(
            "note: auth scheme `{}` skipped: {}",
            o.scheme,
            o.unsupported.as_deref().unwrap_or("unsupported")
        );
    }
    let options = if declared.iter().any(|o| o.config.is_some()) {
        declared
    } else {
        prompt::generic_auth_options(&det.env_prefix)
    };
    let items = prompt::auth_items(&options, |_| None);
    let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
    let default = match flag {
        Some("none") => items.len() - 1,
        Some(name) => items
            .iter()
            .position(|i| i.option.as_ref().is_some_and(|o| o.scheme == name))
            .with_context(|| {
                format!(
                    "--auth {name}: available schemes are {}",
                    options
                        .iter()
                        .filter(|o| o.config.is_some())
                        .map(|o| o.scheme.as_str())
                        .chain(["none"])
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?,
        // With no scheme declared, the safe default is no auth.
        None if det.auth.iter().all(|o| o.config.is_none()) => items.len() - 1,
        None => 0,
    };
    let i = p.select("Authentication", &labels, Some(default))?;
    let Some(opt) = &items[i].option else {
        return Ok((None, Vec::new(), Vec::new()));
    };
    let creds = prompt::ask_credentials(p, &opt.vars, |k| current_value(ws, k))?;
    Ok((opt.config.clone(), opt.vars.clone(), creds))
}

/// The value already known for a variable: `.env` first, then the process env.
pub(crate) fn current_value(ws: &Workspace, key: &str) -> Option<String> {
    envfile::get(&ws.env_path(), key)
        .ok()
        .flatten()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var(key).ok().filter(|v| !v.is_empty()))
}

fn write_files(
    ws: &Workspace,
    spec: &Spec,
    det: &Detection,
    config: &Config,
    baseline: &Config,
    env_updates: &[(String, Option<String>)],
    auth_vars: &[detect::EnvVar],
) -> Result<()> {
    let header = format!(
        "# apisip config for \"{}\", generated by `apisip init`. Edit freely:\n\
         # `apisip update` keeps your edits when it merges a new version of the spec.\n\
         # Secrets live in .env and are referenced here as ${{VAR}}.",
        det.title
    );
    let mut notes = BTreeMap::new();
    for s in &config.streams {
        notes.insert(s.name.clone(), stream_note(s));
    }
    let source_note = config.source.rate_limit.is_none().then_some(
        "# No rate limit was found in the spec. Check the provider's docs or pricing page,\n\
         # then add a line like this under [source]:\n\
         # rate_limit = { max_calls = 100, period = \"1m\" }",
    );
    std::fs::write(
        ws.config_path(),
        config.render(&header, &notes, source_note),
    )?;
    ws.save_meta(
        spec,
        &baseline.render(BASELINE_HEADER, &BTreeMap::new(), None),
    )?;
    ws.folder_id()?;

    // Keys from the auth scheme, with values from the prompts or the process env.
    let mut updates: Vec<(String, Option<String>)> = env_updates.to_vec();
    for (k, v) in updates.iter_mut() {
        if v.is_none() {
            *v = std::env::var(k.as_str()).ok().filter(|v| !v.is_empty());
        }
    }
    envfile::upsert(&ws.env_path(), &updates)?;
    let mut example_keys: Vec<String> = auth_vars.iter().map(|v| v.name.clone()).collect();
    for (k, _) in env_updates {
        if !example_keys.contains(k) {
            example_keys.push(k.clone());
        }
    }
    envfile::write_example(&ws.dir.join(".env.example"), &example_keys)?;
    envfile::ensure_gitignored(&ws.dir.join(".gitignore"), ".env")?;
    envfile::ensure_gitignored(&ws.dir.join(".gitignore"), ".apisip/run.lock")?;
    // Downloaded data inside the folder stays out of git.
    if let Some(rel) = config.destination.url.strip_prefix("file://")
        && !rel.starts_with('/')
        && !rel.contains("..")
        && !rel.contains("${")
        && rel.trim_start_matches("./") != "."
    {
        let rel = rel.trim_start_matches("./").trim_end_matches('/');
        envfile::ensure_gitignored(&ws.dir.join(".gitignore"), &format!("/{rel}/"))?;
    }
    Ok(())
}

pub(crate) fn stream_note(s: &StreamConfig) -> String {
    let mode = match s.mode() {
        crate::config::Mode::Filter => format!(
            "filter: the next run sends {}=<last {}>",
            s.incremental
                .as_ref()
                .and_then(|i| i.param.clone())
                .unwrap_or_default(),
            s.cursor().unwrap_or_default()
        ),
        crate::config::Mode::EarlyStop => format!(
            "early stop: sorted by {} descending, stops at the last watermark",
            s.cursor().unwrap_or_default()
        ),
        crate::config::Mode::FullRefresh => {
            "full refresh: every run downloads a complete snapshot".into()
        }
    };
    format!("# {}: GET {} ({mode})", s.name, s.path)
}

fn print_summary(
    ws: &Workspace,
    det: &Detection,
    config: &Config,
    auth_vars: &[detect::EnvVar],
    env_updates: &[(String, Option<String>)],
) {
    let enabled: Vec<&StreamConfig> = config.streams.iter().filter(|s| s.enabled).collect();
    println!(
        "\nWrote apisip.toml, .env, .env.example and .apisip/ in {}",
        crate::workspace::display_dir(&ws.dir)
    );
    println!("\n{} stream(s) enabled:", enabled.len());
    for s in &enabled {
        let pk = if s.primary_key.is_empty() {
            "no primary key".to_string()
        } else {
            format!("key {}", s.primary_key.join(","))
        };
        println!(
            "  {:<24} {:<13} {:<13} {pk}",
            s.name,
            s.mode().label(),
            pagination_label(&s.pagination)
        );
    }
    if !det.skipped.is_empty() {
        println!("\nSkipped:");
        for s in &det.skipped {
            println!("  {:<38} {}", s.path, s.reason);
        }
    }
    let mut blank: Vec<&str> = auth_vars
        .iter()
        .map(|v| v.name.as_str())
        .chain(env_updates.iter().map(|(k, _)| k.as_str()))
        .filter(|k| current_value(ws, k).is_none())
        .collect();
    blank.dedup();
    if !blank.is_empty() {
        println!("\nFill in {} in .env before running.", blank.join(", "));
    }
    match &config.source.rate_limit {
        Some(r) => println!(
            "\nRate limit: {} calls per {}.",
            r.max_calls,
            humantime::format_duration(r.period)
        ),
        None => {
            println!();
            print_rate_limit_warning(true);
        }
    }
    if det.rate_limits.headers_declared {
        println!("The API reports its remaining budget in headers; apisip will follow them.");
    }
    println!(
        "\nThis folder is the home of this API: run apisip from here, or pass -C <dir>.\n\
         Next:\n  apisip list    # what will be downloaded\n  apisip check   # one request per stream, writes nothing\n  apisip run     # download"
    );
}

fn stream_names(streams: &[StreamConfig]) -> String {
    streams
        .iter()
        .map(|s| s.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Empty -> the default; a path -> `file://<path>`; URLs unchanged.
pub fn normalize_destination(input: &str) -> String {
    let t = input.trim();
    if t.is_empty() {
        return crate::config::DEFAULT_DESTINATION.into();
    }
    if t.contains("://") {
        return t.trim_end_matches('/').to_string();
    }
    format!("file://{}", t.trim_end_matches('/'))
}
