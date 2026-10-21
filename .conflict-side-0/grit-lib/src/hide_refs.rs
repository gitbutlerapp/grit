//! `transfer.hideRefs` / `receive.hideRefs` / `uploadpack.hideRefs` matching (Git `ref_is_hidden`).

/// Collect hide-ref patterns from config for receive-pack (transfer + receive).
#[must_use]
pub fn hide_ref_patterns_receive(config: &crate::config::ConfigSet) -> Vec<String> {
    let mut v = config.get_all("transfer.hideRefs");
    v.extend(config.get_all("receive.hideRefs"));
    normalize_patterns(v)
}

/// Collect hide-ref patterns for upload-pack (transfer + uploadpack).
#[must_use]
pub fn hide_ref_patterns_uploadpack(config: &crate::config::ConfigSet) -> Vec<String> {
    let mut v = config.get_all("transfer.hideRefs");
    v.extend(config.get_all("uploadpack.hideRefs"));
    normalize_patterns(v)
}

fn normalize_patterns(mut pats: Vec<String>) -> Vec<String> {
    for p in &mut pats {
        while p.ends_with('/') && p.len() > 1 {
            p.pop();
        }
    }
    pats
}

/// Whether `refname` (logical) / `refname_full` (storage) is hidden by `patterns`.
///
/// `patterns` are in config order; **later** entries win (matches Git's reverse scan).
#[must_use]
pub fn ref_is_hidden(refname: &str, refname_full: &str, patterns: &[String]) -> bool {
    let mut i = patterns.len();
    while i > 0 {
        i -= 1;
        let mut m = patterns[i].as_str();
        let mut neg = false;
        if let Some(rest) = m.strip_prefix('!') {
            neg = true;
            m = rest;
        }
        let subject = if let Some(rest) = m.strip_prefix('^') {
            m = rest;
            refname_full
        } else {
            refname
        };
        if subject.is_empty() {
            continue;
        }
        if subject.starts_with(m)
            && (m.is_empty()
                || subject.len() == m.len()
                || subject.as_bytes().get(m.len()) == Some(&b'/'))
        {
            return !neg;
        }
    }
    false
}
