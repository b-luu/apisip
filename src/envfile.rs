//! Reading and writing `.env` files without losing comments or ordering.
//! Files are always written with mode 0600.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Parses `KEY=VALUE` lines the same way `dotenvy` does for our own output.
pub fn read(path: &Path) -> Result<Vec<(String, String)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for item in
        dotenvy::from_path_iter(path).with_context(|| format!("reading {}", path.display()))?
    {
        out.push(item.with_context(|| format!("parsing {}", path.display()))?);
    }
    Ok(out)
}

pub fn get(path: &Path, key: &str) -> Result<Option<String>> {
    Ok(read(path)?
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v))
}

/// Sets `Some(value)` keys, and adds `None` keys with an empty value when
/// they are absent. Other lines are kept as they are.
pub fn upsert(path: &Path, updates: &[(String, Option<String>)]) -> Result<()> {
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
    for (key, value) in updates {
        let pos = lines.iter().position(|l| line_key(l) == Some(key.as_str()));
        match (pos, value) {
            (Some(i), Some(v)) => lines[i] = format!("{key}={}", quote(v)),
            (Some(_), None) => {}
            (None, v) => lines.push(format!(
                "{key}={}",
                v.as_deref().map(quote).unwrap_or_default()
            )),
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    write_private(path, text.as_bytes())
}

/// Writes `.env.example`: the same keys, empty values.
pub fn write_example(path: &Path, keys: &[String]) -> Result<()> {
    let mut text =
        String::from("# Copy to .env and fill in. apisip reads .env from the folder it runs in.\n");
    for k in keys {
        text.push_str(&format!("{k}=\n"));
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

fn line_key(line: &str) -> Option<&str> {
    let l = line.trim_start();
    let l = l.strip_prefix("export ").unwrap_or(l);
    if l.starts_with('#') {
        return None;
    }
    l.split_once('=').map(|(k, _)| k.trim())
}

fn quote(v: &str) -> String {
    if v.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@+=,".contains(c))
    {
        v.to_string()
    } else {
        format!(
            "\"{}\"",
            v.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('$', "\\$")
        )
    }
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(0o600);
        let mut f = opts
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        f.write_all(bytes)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let mut f = opts
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        f.write_all(bytes)?;
    }
    Ok(())
}

/// Adds `entry` to a `.gitignore`, creating it if needed.
pub fn ensure_gitignored(path: &Path, entry: &str) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim() == entry || l.trim() == format!("/{entry}"))
    {
        return Ok(());
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(entry);
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// `****abcd` for display; never the whole secret.
pub fn mask(v: &str) -> String {
    if v.is_empty() {
        return "(empty)".into();
    }
    let n = v.chars().count();
    if n >= 12 {
        format!("****{}", v.chars().skip(n - 4).collect::<String>())
    } else {
        "****".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_keeps_comments_and_quotes_values() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env");
        std::fs::write(&p, "# mine\nA=1\n").unwrap();
        upsert(
            &p,
            &[
                ("A".into(), Some("two words".into())),
                ("B".into(), None),
                ("C".into(), Some("p\"$x".into())),
            ],
        )
        .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# mine\n"));
        let parsed = read(&p).unwrap();
        assert_eq!(
            parsed,
            [
                ("A".into(), "two words".into()),
                ("B".into(), String::new()),
                ("C".into(), "p\"$x".into())
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn masking() {
        assert_eq!(mask("sk_live_1234567890abcd"), "****abcd");
        assert_eq!(mask("short"), "****");
        assert_eq!(mask(""), "(empty)");
    }
}
