//! URL rewrite helpers for `url.*.insteadOf` / `url.*.pushInsteadOf`.
//!
//! Git applies these rewrites when resolving transport URLs. Fetch reads
//! `insteadOf` rules only, while push prefers `pushInsteadOf` and falls back
//! to `insteadOf`.

use crate::config::ConfigSet;

fn rewrite_by_suffix(config: &ConfigSet, url: &str, suffix: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for entry in config.entries() {
        let Some(base) = entry
            .key
            .strip_prefix("url.")
            .and_then(|s| s.strip_suffix(suffix))
        else {
            continue;
        };
        let Some(instead_of) = entry.value.as_deref() else {
            continue;
        };
        if !url.starts_with(instead_of) {
            continue;
        }
        let match_len = instead_of.len();
        if best.as_ref().is_none_or(|(len, _)| match_len > *len) {
            best = Some((match_len, base.to_owned()));
        }
    }
    best.map(|(match_len, base)| format!("{base}{}", &url[match_len..]))
}

/// Rewrite a URL for fetch semantics (`url.<base>.insteadOf`).
#[must_use]
pub fn rewrite_fetch_url(config: &ConfigSet, url: &str) -> String {
    rewrite_by_suffix(config, url, ".insteadof").unwrap_or_else(|| url.to_owned())
}

/// Rewrite a URL for push semantics (`pushInsteadOf` first, then `insteadOf`).
#[must_use]
pub fn rewrite_push_url(config: &ConfigSet, url: &str) -> String {
    rewrite_by_suffix(config, url, ".pushinsteadof")
        .or_else(|| rewrite_by_suffix(config, url, ".insteadof"))
        .unwrap_or_else(|| url.to_owned())
}
