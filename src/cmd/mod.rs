pub mod init;
pub mod list;

use crate::config::Pagination;

pub fn pagination_label(p: &Option<Pagination>) -> &'static str {
    match p {
        None => "single page",
        Some(Pagination::PageNumber { .. }) => "page number",
        Some(Pagination::Offset { .. }) => "offset",
        Some(Pagination::Cursor { .. }) => "cursor",
        Some(Pagination::NextUrl { .. }) => "next url",
        Some(Pagination::LinkHeader) => "link header",
    }
}

/// The warning printed when no static rate limit is configured.
pub fn print_rate_limit_warning(long: bool) {
    if long {
        eprintln!(
            "warning: no rate limit was found in the spec, so none is configured.\n\
             apisip still backs off on 429 responses and follows rate-limit headers, but a\n\
             static limit avoids hitting the API too hard. Check the provider's docs or\n\
             pricing page, then add under [source] in apisip.toml:\n\
             \n    rate_limit = {{ max_calls = 100, period = \"1m\" }}\n"
        );
    } else {
        eprintln!(
            "warning: no rate_limit in apisip.toml; add `rate_limit = {{ max_calls = N, period = \"1m\" }}` under [source]"
        );
    }
}
