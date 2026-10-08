//! Reference exclusion rules for `rev-list` / `rev-parse` (`--exclude`, `--exclude-hidden`).
//!
//! Mirrors Git's `ref_exclusions` / `parse_hide_refs_config` / `ref_is_hidden` in `revision.c`
//! and `refs.c`.

use crate::config::ConfigSet;
use crate::wildmatch::wildmatch;

/// One `transfer.hideRefs` / `<section>.hideRefs` prefix rule (after normalization).
#[derive(Debug, Clone)]
struct HideRefRule {
    /// Pattern without leading `!` or `^`.
    pattern: String,
    /// If true, the rule negates a previous hide (Git `!` prefix).
    negated: bool,
    /// If true, match against the full ref name (`^` prefix in config).
    full_ref: bool,
}

/// Patterns that exclude refs from `--all` / glob expansion, including hidden-ref config.
#[derive(Debug, Clone, Default)]
pub struct RefExclusions {
    /// `wildmatch` patterns from `--exclude=<pat>` (full ref names).
    excluded_refs: Vec<String>,
    /// Rules from config when `--exclude-hidden=<section>` is active.
    hidden_rules: Vec<HideRefRule>,
    /// Set after `--exclude-hidden=` is parsed; cleared by [`RefExclusions::clear`].
    /// Used to reject a second `--exclude-hidden` before the next pseudo-ref clears state.
    pub hidden_configured: bool,
}

impl RefExclusions {
    /// Reset exclusions after `--all` / `--glob` / `--branches` / … (matches Git `clear_ref_exclusions`).
    pub fn clear(&mut self) {
        self.excluded_refs.clear();
        self.hidden_rules.clear();
        self.hidden_configured = false;
    }

    /// Append a `--exclude=<pattern>` entry (Git wildmatch on the ref name).
    pub fn add_excluded_ref(&mut self, pattern: impl Into<String>) {
        self.excluded_refs.push(pattern.into());
    }

    /// Load `transfer.hideRefs` and `<section>.hideRefs` into this set.
    ///
    /// `section` must be one of `fetch`, `receive`, or `uploadpack`.
    pub fn load_hidden_refs_from_config(&mut self, config: &ConfigSet, section: &str) {
        self.hidden_configured = true;
        let section_key = format!("{section}.hiderefs");
        for e in config.entries() {
            if e.key == "transfer.hiderefs" || e.key == section_key {
                if let Some(v) = e.value.as_deref() {
                    self.hidden_rules.push(parse_hide_refs_value(v));
                }
            }
        }
    }

    /// Whether this ref should be omitted from ref listing (exclude + hidden rules).
    ///
    /// - `stripped_name` — ref name with `GIT_NAMESPACE` prefix removed, when applicable.
    /// - `full_name` — storage path of the ref (e.g. `refs/heads/main`).
    pub fn ref_excluded(&self, stripped_name: Option<&str>, full_name: &str) -> bool {
        for pat in &self.excluded_refs {
            if wildmatch(pat.as_bytes(), full_name.as_bytes(), 0) {
                return true;
            }
        }
        ref_is_hidden(stripped_name, full_name, &self.hidden_rules)
    }
}

fn trim_trailing_slashes(mut s: String) -> String {
    while s.ends_with('/') {
        s.pop();
    }
    s
}

fn parse_hide_refs_value(raw: &str) -> HideRefRule {
    let mut rest = raw;
    let mut negated = false;
    if let Some(stripped) = rest.strip_prefix('!') {
        negated = true;
        rest = stripped;
    }
    let mut full_ref = false;
    if let Some(stripped) = rest.strip_prefix('^') {
        full_ref = true;
        rest = stripped;
    }
    HideRefRule {
        pattern: trim_trailing_slashes(rest.to_owned()),
        negated,
        full_ref,
    }
}

fn ref_is_hidden(stripped_name: Option<&str>, full_name: &str, rules: &[HideRefRule]) -> bool {
    for rule in rules.iter().rev() {
        let subject = if rule.full_ref {
            full_name
        } else {
            match stripped_name {
                Some(s) => s,
                None => continue,
            }
        };
        if subject.is_empty() {
            continue;
        }
        let pat = rule.pattern.as_str();
        if pat.is_empty() {
            continue;
        }
        if skip_prefix_git(subject, pat)
            .is_some_and(|tail| tail.is_empty() || tail.starts_with('/'))
        {
            return !rule.negated;
        }
    }
    false
}

/// Git `skip_prefix` semantics: `subject` must begin with `prefix` byte-for-byte; returns the tail.
fn skip_prefix_git<'a>(subject: &'a str, prefix: &str) -> Option<&'a str> {
    let b = subject.as_bytes();
    let p = prefix.as_bytes();
    if p.is_empty() {
        return Some(subject);
    }
    if b.len() < p.len() {
        return None;
    }
    if &b[..p.len()] == p {
        subject.get(p.len()..)
    } else {
        None
    }
}

/// `GIT_NAMESPACE` value (e.g. `namespace` / `a/b`) expanded to the `refs/namespaces/.../`
/// prefix, or empty string when unset.
pub fn git_namespace_prefix(env: &crate::environment::Environment) -> String {
    let raw = env.git_namespace.clone().unwrap_or_default();
    if raw.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for comp in raw.split('/') {
        if comp.is_empty() {
            continue;
        }
        out.push_str("refs/namespaces/");
        out.push_str(comp);
        out.push('/');
    }
    while out.ends_with('/') {
        out.pop();
    }
    if !out.is_empty() {
        out.push('/');
    }
    out
}

/// Strip a leading namespace prefix from `refname`, returning `None` when not under the namespace.
pub fn strip_git_namespace<'a>(refname: &'a str, namespace_prefix: &str) -> Option<&'a str> {
    if namespace_prefix.is_empty() {
        return Some(refname);
    }
    refname.strip_prefix(namespace_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hide_refs_prefix_match() {
        let rules = vec![parse_hide_refs_value("refs/hidden/")];
        assert!(ref_is_hidden(
            Some("refs/hidden/foo"),
            "refs/hidden/foo",
            &rules
        ));
        assert!(!ref_is_hidden(
            Some("refs/heads/main"),
            "refs/heads/main",
            &rules
        ));
    }

    #[test]
    fn hide_refs_negation() {
        let rules = vec![
            parse_hide_refs_value("refs/foo/"),
            parse_hide_refs_value("!refs/foo/bar"),
        ];
        assert!(!ref_is_hidden(Some("refs/foo/bar"), "refs/foo/bar", &rules));
        assert!(ref_is_hidden(Some("refs/foo/baz"), "refs/foo/baz", &rules));
    }
}
