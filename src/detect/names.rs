//! Names derived from the spec: stream names and the env variable prefix.

/// A key or parameter name reduced to lowercase letters and digits, to
/// compare names across spellings.
pub(super) fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '$')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// `/v1/users/search` -> `users_search`.
pub fn stream_name(path: &str) -> String {
    let parts: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .filter(|s| {
            let l = s.to_ascii_lowercase();
            !(l == "api"
                || l == "rest"
                || (l.starts_with('v')
                    && l[1..].chars().all(|c| c.is_ascii_digit() || c == '.')
                    && l.len() > 1))
        })
        .map(snake)
        .collect();
    let name = parts.join("_");
    if name.is_empty() { "root".into() } else { name }
}

pub fn snake(s: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        } else {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            prev_lower = false;
        }
    }
    out.trim_matches('_').to_string()
}

/// Env var prefix from the spec title: "Acme Billing REST API v2" -> "ACME_BILLING".
pub fn env_prefix(title: &str) -> String {
    const NOISE: [&str; 8] = [
        "api",
        "apis",
        "openapi",
        "swagger",
        "rest",
        "spec",
        "specification",
        "the",
    ];
    let words: Vec<String> = title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .filter(|w| !NOISE.contains(&w.to_ascii_lowercase().as_str()))
        .filter(|w| !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| {
            let l = w.to_ascii_lowercase();
            !(l.starts_with('v') && l.len() > 1 && l[1..].chars().all(|c| c.is_ascii_digit()))
        })
        .take(2)
        .map(|w| w.to_ascii_uppercase())
        .collect();
    if words.is_empty() {
        "API".into()
    } else {
        words.join("_")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_prefixes() {
        assert_eq!(stream_name("/v1/users/search"), "users_search");
        assert_eq!(stream_name("/api/v2.1/LineItems"), "line_items");
        assert_eq!(stream_name("/"), "root");
        assert_eq!(env_prefix("Swagger Petstore - OpenAPI 3.0"), "PETSTORE");
        assert_eq!(env_prefix("GitHub v3 REST API"), "GITHUB");
        assert_eq!(env_prefix("!!!"), "API");
    }
}
