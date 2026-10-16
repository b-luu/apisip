//! Interactive prompts shared by `init` and `update`. Every prompt has a
//! default, and `--yes` takes it without asking.

use std::io::IsTerminal;

use anyhow::{Result, bail};
use inquire::{MultiSelect, Password, PasswordDisplayMode, Select, Text};

use crate::config::AuthConfig;
use crate::detect::{AuthOption, EnvVar};

#[derive(Debug, Clone, Copy)]
pub struct Prompter {
    pub interactive: bool,
}

impl Prompter {
    pub fn new(yes: bool) -> Result<Prompter> {
        if !yes && !std::io::stdin().is_terminal() {
            bail!("no terminal to prompt on: pass --yes (and flags for the values you need)");
        }
        Ok(Prompter { interactive: !yes })
    }

    pub fn text(&self, message: &str, default: &str, help: Option<&str>) -> Result<String> {
        if !self.interactive {
            return Ok(default.to_string());
        }
        let mut p = Text::new(message);
        if !default.is_empty() {
            p = p.with_default(default);
        }
        if let Some(h) = help {
            p = p.with_help_message(h);
        }
        Ok(p.prompt()?.trim().to_string())
    }

    /// A secret, typed without echo. Empty input returns `None` (keep the current value).
    pub fn secret(&self, message: &str, help: &str) -> Result<Option<String>> {
        if !self.interactive {
            return Ok(None);
        }
        let v = Password::new(message)
            .without_confirmation()
            .with_display_mode(PasswordDisplayMode::Masked)
            .with_help_message(help)
            .prompt()?;
        Ok((!v.is_empty()).then_some(v))
    }

    pub fn select(
        &self,
        message: &str,
        options: &[String],
        default: Option<usize>,
    ) -> Result<usize> {
        if !self.interactive {
            return default
                .ok_or_else(|| anyhow::anyhow!("{message}: no default, a choice is required"));
        }
        let picked = Select::new(message, options.to_vec())
            .with_starting_cursor(default.unwrap_or(0))
            .raw_prompt()?;
        Ok(picked.index)
    }

    pub fn multi_select(
        &self,
        message: &str,
        options: &[String],
        defaults: &[usize],
    ) -> Result<Vec<usize>> {
        if !self.interactive {
            return Ok(defaults.to_vec());
        }
        let picked = MultiSelect::new(message, options.to_vec())
            .with_default(defaults)
            .with_help_message("space to toggle, enter to confirm")
            .raw_prompt()?;
        Ok(picked.into_iter().map(|o| o.index).collect())
    }
}

/// One entry of the auth picker.
#[derive(Debug, Clone)]
pub struct AuthItem {
    pub label: String,
    /// `None` is "no authentication".
    pub option: Option<AuthOption>,
}

pub fn auth_items(
    options: &[AuthOption],
    tag: impl Fn(&AuthOption) -> Option<&'static str>,
) -> Vec<AuthItem> {
    let mut items: Vec<AuthItem> = options
        .iter()
        .filter(|o| o.config.is_some())
        .map(|o| {
            let kind = o.config.as_ref().map(AuthConfig::kind).unwrap_or("");
            let mut label = format!("{} ({kind})", o.scheme);
            if !o.description.is_empty() {
                label.push_str(&format!(" - {}", truncate(&o.description, 50)));
            }
            if let Some(t) = tag(o) {
                label.push_str(&format!(" [{t}]"));
            }
            AuthItem {
                label,
                option: Some(o.clone()),
            }
        })
        .collect();
    items.push(AuthItem {
        label: "none (public API)".into(),
        option: None,
    });
    items
}

/// Asks for each credential variable. `current` gives the value already in
/// `.env`; Enter keeps it. Returns `(name, Some(new value) | None to keep)`.
pub fn ask_credentials(
    p: &Prompter,
    vars: &[EnvVar],
    current: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, Option<String>)>> {
    let mut out = Vec::new();
    for v in vars {
        let cur = current(&v.name).unwrap_or_default();
        let value = if v.secret {
            let help = if cur.is_empty() {
                "stored in .env (mode 600); leave empty to fill in later".to_string()
            } else {
                format!(
                    "Enter keeps the current value {}",
                    crate::envfile::mask(&cur)
                )
            };
            p.secret(&format!("{} ({})", v.label, v.name), &help)?
        } else {
            let t = p.text(
                &format!("{} ({})", v.label, v.name),
                &cur,
                Some("stored in .env"),
            )?;
            (t != cur).then_some(t)
        };
        out.push((v.name.clone(), value));
    }
    Ok(out)
}

/// Supported schemes from the spec, or generic choices when it declares none.
pub fn generic_auth_options(prefix: &str) -> Vec<AuthOption> {
    use crate::config::ApiKeyLocation;
    let var = |s: &str, label: &str, secret| EnvVar {
        name: format!("{prefix}_{s}"),
        label: label.into(),
        secret,
    };
    vec![
        AuthOption {
            scheme: "bearer".into(),
            description: "Authorization: Bearer <token>".into(),
            config: Some(AuthConfig::Bearer {
                scheme: None,
                token: format!("${{{prefix}_TOKEN}}"),
            }),
            unsupported: None,
            vars: vec![var("TOKEN", "Bearer token", true)],
        },
        AuthOption {
            scheme: "api_key".into(),
            description: "X-API-Key header".into(),
            config: Some(AuthConfig::ApiKey {
                scheme: None,
                name: "X-API-Key".into(),
                location: ApiKeyLocation::Header,
                value: format!("${{{prefix}_API_KEY}}"),
            }),
            unsupported: None,
            vars: vec![var("API_KEY", "API key", true)],
        },
        AuthOption {
            scheme: "basic".into(),
            description: "HTTP basic".into(),
            config: Some(AuthConfig::Basic {
                scheme: None,
                username: format!("${{{prefix}_USERNAME}}"),
                password: format!("${{{prefix}_PASSWORD}}"),
            }),
            unsupported: None,
            vars: vec![
                var("USERNAME", "Username", false),
                var("PASSWORD", "Password", true),
            ],
        },
    ]
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
