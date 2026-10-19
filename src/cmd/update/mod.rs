//! `apisip update`: re-detect from a new version of the spec and merge the
//! result into apisip.toml, three ways:
//!
//! - base: `.apisip/baseline.toml`, what detection produced last time;
//! - theirs: what detection produces from the new spec;
//! - mine: the current apisip.toml.
//!
//! A value the user never edited follows the spec. A value both changed is a
//! conflict and is asked about (`--yes` keeps the user's). Removed endpoints
//! are disabled, new ones are offered unselected, and auth is re-confirmed.

mod auth;
mod config_merge;
mod report;
mod schema_merge;

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use crate::config::{AuthConfig, Config};
use crate::detect::{self, AuthOption, Detection, EnvVar};
use crate::envfile;
use crate::prompt::{self, Prompter};
use crate::sink::Store;
use crate::spec::Spec;
use crate::workspace::Workspace;

use super::init::{BASELINE_HEADER, baseline, current_value};
use auth::{auth_names, confirm_auth};
use config_merge::{Diff, Kind, Resolution, apply, changes_views, diff};
use report::{field_short, print_diff, print_drift, short};
use schema_merge::{FieldMerge, StreamSchema, plan_schemas, recent_undeclared};

#[derive(Debug, Default, Clone)]
pub struct UpdateOptions {
    pub spec: Option<String>,
    pub check: bool,
    pub yes: bool,
}

/// Returns false when `--check` found changes (exit code 1).
pub async fn run(ws: &Workspace, o: UpdateOptions) -> Result<bool> {
    let plan = Plan::load(ws, &o).await?;
    plan.print().await;
    if o.check {
        if !plan.changed() {
            println!("No change: apisip.toml matches the spec.");
        }
        return Ok(!plan.changed());
    }

    let p = Prompter::new(o.yes)?;
    if !p.interactive {
        plan.refuse_undecided()?;
    }
    let mut r = plan.settle_config(&p)?;
    let schema_picks = plan.settle_schemas(&p)?;
    let (auth, vars) = confirm_auth(
        &p,
        &plan.det,
        &plan.base,
        &plan.mine,
        plan.old_auth.as_deref(),
    )?;
    if auth != plan.mine.source.auth {
        r.auth = Some(auth.clone());
    }
    let creds = prompt::ask_credentials(&p, &vars, |k| current_value(ws, k))?;

    let merged = apply(&plan.text, &plan.d, &plan.theirs, &r)?;
    let wrote = merged != plan.text;
    if wrote {
        std::fs::write(ws.config_path(), &merged)?;
    }
    plan.write_baseline(ws, auth)?;
    let kept = plan.write_schemas(ws, &schema_picks)?;
    write_env(ws, creds, &vars)?;

    println!(
        "\n{} Spec snapshot and baseline refreshed in .apisip/.",
        if wrote {
            "Updated apisip.toml."
        } else {
            "apisip.toml unchanged."
        }
    );
    if !plan.d.removed.is_empty() {
        println!(
            "Disabled (gone from the spec, data and watermarks kept): {}",
            plan.d.removed.join(", ")
        );
    }
    for line in kept {
        println!("{line}");
    }
    Ok(true)
}

/// Everything update compares: the three sides of apisip.toml, the schema
/// files and the auth schemes of the old and new spec.
struct Plan {
    text: String,
    mine: Config,
    base: Config,
    theirs: Config,
    new_spec: Spec,
    det: Detection,
    d: Diff,
    store: Option<Store>,
    schemas: Vec<StreamSchema>,
    spec_change: Option<String>,
    old_auth: Option<Vec<AuthOption>>,
    auth_changed: bool,
}

/// Per schema file: which changes to take, and which undeclared fields to adopt.
type SchemaPicks = Vec<(Vec<bool>, Vec<usize>)>;

impl Plan {
    async fn load(ws: &Workspace, o: &UpdateOptions) -> Result<Plan> {
        let text = ws.read_config_text()?;
        let mine = ws.load_config()?;
        let base_path = ws.baseline_path();
        let base_text = std::fs::read_to_string(&base_path).with_context(|| {
            format!(
                "reading {}: update needs the baseline `apisip init` writes. Re-create it with `apisip init --force`",
                base_path.display()
            )
        })?;
        let base = Config::parse(&base_text).context("invalid .apisip/baseline.toml")?;
        let old_spec = ws.load_spec(&mine).await.ok();

        let new_spec = match &o.spec {
            Some(s) => Spec::load(s, &std::env::current_dir()?).await?,
            None => Spec::load(&mine.source.spec, &ws.dir).await?,
        };
        let det = detect::detect(&new_spec);
        let theirs = baseline(&det, &mine.source);
        let d = diff(&base, &theirs, &mine);

        let store = super::open_existing_store(ws, &mine);
        let undeclared = match &store {
            Some(s) => recent_undeclared(s).await,
            None => BTreeMap::new(),
        };
        let schemas = plan_schemas(ws, &mine, &theirs, &d, &new_spec, &undeclared)?;
        let spec_change = o.spec.as_ref().filter(|s| **s != mine.source.spec).cloned();
        let old_auth = old_spec.as_ref().map(|s| detect::detect(s).auth);
        let auth_changed = old_auth
            .as_ref()
            .is_some_and(|old| auth_names(old) != auth_names(&det.auth));

        Ok(Plan {
            text,
            mine,
            base,
            theirs,
            new_spec,
            det,
            d,
            store,
            schemas,
            spec_change,
            old_auth,
            auth_changed,
        })
    }

    async fn print(&self) {
        print_diff(
            &self.d,
            &self.schemas,
            self.auth_changed
                .then_some((&self.old_auth, &self.det.auth)),
        );
        if let Some(store) = &self.store {
            print_drift(store, &self.mine, &self.new_spec).await;
        }
    }

    fn changed(&self) -> bool {
        !self.d.is_empty()
            || self.schemas.iter().any(StreamSchema::changed)
            || self.auth_changed
            || self.spec_change.is_some()
    }

    /// Without a terminal, a change that needs a decision stops the update
    /// before anything is written.
    fn refuse_undecided(&self) -> Result<()> {
        let (d, schemas) = (&self.d, &self.schemas);
        let mut blocking: Vec<String> = d
            .fields
            .iter()
            .filter(|c| c.kind == Kind::Update && changes_views(c))
            .map(|c| {
                format!(
                    "streams.{}.{}: {} -> {}",
                    c.stream.as_deref().unwrap_or_default(),
                    c.key,
                    short(&c.mine),
                    short(&c.theirs)
                )
            })
            .collect();
        for s in schemas {
            for c in &s.changes {
                if c.kind == Kind::Update && !c.compatible {
                    blocking.push(format!("schemas/{}.json: {}", s.name, c.describe()));
                }
            }
        }
        if !blocking.is_empty() {
            bail!(
                "nothing was written: these spec changes do not match the data already written and need a decision:\n  {}\nRun `apisip update` in a terminal to choose.",
                blocking.join("\n  ")
            );
        }
        Ok(())
    }

    /// Asks about each config change that needs a decision, and the new endpoints.
    fn settle_config(&self, p: &Prompter) -> Result<Resolution> {
        let (d, theirs) = (&self.d, &self.theirs);
        let mut r = Resolution {
            spec: self.spec_change.clone(),
            ..Default::default()
        };
        for (i, c) in d.fields.iter().enumerate() {
            match c.kind {
                Kind::Update if changes_views(c) => {
                    let options = vec![
                        format!("keep the current one: {}", short(&c.mine)),
                        format!("take the spec's: {}", short(&c.theirs)),
                    ];
                    let pick = p.select(
                        &format!(
                            "streams.{}.{} changed in the spec; the views deduplicate on it",
                            c.stream.as_deref().unwrap_or_default(),
                            c.key
                        ),
                        &options,
                        Some(0),
                    )?;
                    if pick == 1 {
                        r.take.insert(i);
                    }
                }
                Kind::Update => {
                    r.take.insert(i);
                }
                Kind::AlreadyApplied => {}
                Kind::Conflict => {
                    let options = vec![
                        format!("keep mine: {}", short(&c.mine)),
                        format!("take the spec's: {}", short(&c.theirs)),
                    ];
                    let what = match &c.stream {
                        Some(s) => format!("streams.{s}.{}", c.key),
                        None => format!("source.{}", c.key),
                    };
                    let pick = p.select(
                        &format!("{what} was edited here and changed in the spec"),
                        &options,
                        Some(0),
                    )?;
                    if pick == 1 {
                        r.take.insert(i);
                    }
                }
            }
        }
        if !d.added.is_empty() {
            let labels: Vec<String> = d
                .added
                .iter()
                .map(|n| {
                    let s = theirs.streams.iter().find(|s| &s.name == n).unwrap();
                    format!("{n:<24} GET {:<32} {}", s.path, s.mode().label())
                })
                .collect();
            let picked = p.multi_select("New endpoints to download", &labels, &[])?;
            r.enable = picked.into_iter().map(|i| d.added[i].clone()).collect();
        }
        Ok(r)
    }

    /// Asks about each schema change that needs a decision, and the
    /// undeclared fields to adopt.
    fn settle_schemas(&self, p: &Prompter) -> Result<SchemaPicks> {
        let mut schema_picks: Vec<(Vec<bool>, Vec<usize>)> = Vec::new();
        for s in &self.schemas {
            let mut take = Vec::new();
            for c in &s.changes {
                take.push(match c.kind {
                    Kind::AlreadyApplied => false,
                    Kind::Update if c.compatible => true,
                    Kind::Update => {
                        let options = vec![
                            format!("keep the current one: {}", field_short(&c.mine)),
                            format!("take the spec's: {}", field_short(&c.theirs)),
                        ];
                        let message = format!(
                            "schemas/{}.json: {} (the files already written keep the old type)",
                            s.name,
                            c.describe().trim_start_matches("~ ")
                        );
                        p.select(&message, &options, Some(0))? == 1
                    }
                    Kind::Conflict => {
                        let options = vec![
                            format!("keep mine: {}", field_short(&c.mine)),
                            format!("take the spec's: {}", field_short(&c.theirs)),
                        ];
                        let message = format!(
                            "schemas/{}.json: {} was edited here and changed in the spec",
                            s.name, c.field
                        );
                        p.select(&message, &options, Some(0))? == 1
                    }
                });
            }
            let adopt = if s.undeclared.is_empty() {
                Vec::new()
            } else {
                let labels: Vec<String> = s
                    .undeclared
                    .iter()
                    .map(|(f, shape)| format!("{f} ({})", shape.label()))
                    .collect();
                p.multi_select(
                    &format!(
                        "Declare in schemas/{}.json the fields each recent run saw",
                        s.name
                    ),
                    &labels,
                    &[],
                )?
            };
            schema_picks.push((take, adopt));
        }
        Ok(schema_picks)
    }

    fn write_baseline(&self, ws: &Workspace, auth: Option<AuthConfig>) -> Result<()> {
        let theirs = &self.theirs;
        let new_base = Config {
            source: crate::config::Source {
                auth,
                ..theirs.source.clone()
            },
            ..theirs.clone()
        };
        ws.save_meta(
            &self.new_spec,
            &new_base.render(BASELINE_HEADER, &BTreeMap::new(), None),
        )?;
        Ok(())
    }

    /// Writes the merged schema files. Returns a note per file that keeps
    /// some of the user's version.
    fn write_schemas(&self, ws: &Workspace, schema_picks: &SchemaPicks) -> Result<Vec<String>> {
        let mut kept = Vec::new();
        for (s, (take, adopt)) in self.schemas.iter().zip(schema_picks) {
            let Some(m) = &s.mine else {
                if let Some(t) = &s.theirs {
                    ws.write_schema(&s.name, t)?;
                }
                continue;
            };
            let taken: Vec<&FieldMerge> = s
                .changes
                .iter()
                .zip(take)
                .filter(|(_, t)| **t)
                .map(|(c, _)| c)
                .collect();
            let all_taken = taken.len()
                == s.changes
                    .iter()
                    .filter(|c| c.kind != Kind::AlreadyApplied)
                    .count();
            let mut out = match &s.theirs {
                Some(t) if all_taken && s.base.as_ref() == Some(m) => t.clone(),
                _ => schema_merge::apply(m, &taken),
            };
            for i in adopt {
                let (f, shape) = &s.undeclared[*i];
                schema_merge::adopt(&mut out, f, shape);
            }
            let not_taken: Vec<&str> = s
                .changes
                .iter()
                .zip(take)
                .filter(|(c, t)| c.kind != Kind::AlreadyApplied && !**t)
                .map(|(c, _)| c.field.as_str())
                .collect();
            if !not_taken.is_empty() {
                kept.push(format!(
                    "schemas/{}.json keeps your version of: {}",
                    s.name,
                    not_taken.join(", ")
                ));
            }
            match &s.theirs {
                Some(t) => ws.write_schema_files(&s.name, &out, t)?,
                None if out != *m => {
                    let base = s.base.as_ref().unwrap_or(m);
                    ws.write_schema_files(&s.name, &out, base)?
                }
                None => {}
            }
        }
        Ok(kept)
    }
}

fn write_env(ws: &Workspace, creds: Vec<(String, Option<String>)>, vars: &[EnvVar]) -> Result<()> {
    let updates: Vec<(String, Option<String>)> =
        creds.into_iter().filter(|(_, v)| v.is_some()).collect();
    if !updates.is_empty() {
        envfile::upsert(&ws.env_path(), &updates)?;
    }
    let mut example_keys: Vec<String> = envfile::read(&ws.dir.join(".env.example"))
        .unwrap_or_default()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    for v in vars {
        if !example_keys.contains(&v.name) {
            example_keys.push(v.name.clone());
        }
    }
    envfile::write_example(&ws.dir.join(".env.example"), &example_keys)?;
    Ok(())
}
