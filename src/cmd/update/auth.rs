//! Auth on update: always re-confirmed, and the variables in use are kept
//! when only their names changed in the spec.

use std::collections::BTreeSet;

use anyhow::{Result, bail};

use crate::config::{AuthConfig, Config, referenced_vars};
use crate::detect::{AuthOption, Detection, EnvVar};
use crate::prompt::{self, Prompter};

pub(super) fn auth_names(options: &[AuthOption]) -> BTreeSet<String> {
    options
        .iter()
        .filter(|o| o.config.is_some())
        .map(|o| o.scheme.clone())
        .collect()
}

/// Auth is always re-confirmed: the current scheme is preselected and each
/// credential shows its masked value, so Enter keeps everything.
pub(super) fn confirm_auth(
    p: &Prompter,
    det: &Detection,
    base: &Config,
    mine: &Config,
    old: Option<&[AuthOption]>,
) -> Result<(Option<AuthConfig>, Vec<EnvVar>)> {
    let options: Vec<AuthOption> = if det.auth.iter().any(|o| o.config.is_some()) {
        det.auth.clone()
    } else {
        prompt::generic_auth_options(&det.env_prefix)
    };
    let current = mine.source.auth.as_ref();
    let matches = |o: &AuthOption| match (current, &o.config) {
        (Some(c), Some(oc)) => match c.scheme() {
            Some(name) => name == o.scheme,
            None => c.kind() == oc.kind() && oc.scheme().is_none(),
        },
        _ => false,
    };
    let old_names = old.map(auth_names);
    let items = prompt::auth_items(&options, |o| {
        if matches(o) {
            Some("current")
        } else if old_names.as_ref().is_some_and(|n| !n.contains(&o.scheme)) {
            Some("new")
        } else {
            None
        }
    });
    let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
    let default = match current {
        None => Some(items.len() - 1),
        Some(_) => items
            .iter()
            .position(|i| i.option.as_ref().is_some_and(&matches)),
    };
    if let (Some(c), None) = (current, default) {
        let name = c.scheme().unwrap_or(c.kind());
        eprintln!("The auth scheme in use, `{name}`, is no longer in the spec [removed].");
        if !p.interactive {
            bail!(
                "the auth scheme `{name}` was removed from the spec: run `apisip update` in a terminal to pick another"
            );
        }
    }
    let i = p.select(
        "Authentication (Enter keeps the current one)",
        &labels,
        default,
    )?;
    let Some(opt) = &items[i].option else {
        return Ok((None, Vec::new()));
    };
    let detected = opt.config.clone().unwrap();
    if !matches(opt) {
        return Ok((Some(detected), opt.vars.clone()));
    }
    let cur = current.cloned().unwrap();
    let (auth, vars) = same_scheme(base.source.auth.as_ref(), cur, detected, &opt.vars);
    Ok((Some(auth), vars))
}

/// Same scheme: follow the spec unless the user edited the auth table. The
/// current variable names are kept when only they differ (the spec title
/// they derive from changed), so `.env` keeps working.
fn same_scheme(
    base: Option<&AuthConfig>,
    cur: AuthConfig,
    detected: AuthConfig,
    known: &[EnvVar],
) -> (AuthConfig, Vec<EnvVar>) {
    let edited = base != Some(&cur);
    if cur == detected || (!edited && !same_but_vars(&cur, &detected)) {
        return (detected, known.to_vec());
    }
    let vars = auth_vars(&cur, known);
    (cur, vars)
}

fn same_but_vars(a: &AuthConfig, b: &AuthConfig) -> bool {
    let re = regex::Regex::new(r"\$\{[A-Za-z_][A-Za-z0-9_]*\}").unwrap();
    let shape = |c: &AuthConfig| {
        re.replace_all(&serde_json::to_string(c).unwrap_or_default(), "${}")
            .into_owned()
    };
    shape(a) == shape(b)
}

/// The variables an edited auth table references.
fn auth_vars(auth: &AuthConfig, known: &[EnvVar]) -> Vec<EnvVar> {
    let text = serde_json::to_string(auth).unwrap_or_default();
    let mut out = Vec::new();
    for name in referenced_vars(&text) {
        if out.iter().any(|v: &EnvVar| v.name == name) {
            continue;
        }
        out.push(
            known
                .iter()
                .find(|v| v.name == name)
                .cloned()
                .unwrap_or_else(|| EnvVar {
                    secret: !(name.ends_with("USERNAME") || name.ends_with("CLIENT_ID")),
                    label: name.clone(),
                    name,
                }),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_renamed_spec_keeps_the_auth_variables_in_use() {
        let auth = |var: &str, header: &str| -> AuthConfig {
            toml::from_str(&format!(
                "type = \"api_key\"\nin = \"header\"\nname = \"{header}\"\nvalue = \"${{{var}}}\""
            ))
            .unwrap()
        };
        let old = auth("OLD_API_KEY", "X-Key");
        let known = [EnvVar {
            name: "NEW_API_KEY".into(),
            label: "API key".into(),
            secret: true,
        }];
        // Only the variable name changed: keep the one .env defines.
        let (kept, vars) = same_scheme(
            Some(&old),
            old.clone(),
            auth("NEW_API_KEY", "X-Key"),
            &known,
        );
        assert_eq!(kept, old);
        assert_eq!(
            vars.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["OLD_API_KEY"]
        );
        // A real change in the spec is followed.
        let moved = auth("NEW_API_KEY", "X-Api-Key");
        let (taken, _) = same_scheme(Some(&old), old.clone(), moved.clone(), &known);
        assert_eq!(taken, moved);
    }
}
