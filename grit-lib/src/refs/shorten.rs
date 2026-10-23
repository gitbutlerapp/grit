//! Unambiguous ref shortening (`refs_shorten_unambiguous_ref` semantics).

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::diagnostics::Warning;
use crate::error::{Error, Result};
use crate::refs::store::RawRef;
use crate::refs::SYMREF_MAXDEPTH;
use crate::repo::Repository;
use crate::worktree_ref::DWIM_RULES;

/// Rev-parse DWIM rules in Git precedence order (see `ref_rev_parse_rules` in upstream Git).
const REV_PARSE_RULES: &[&str] = &[
    "{short}",
    "refs/{short}",
    "refs/tags/{short}",
    "refs/heads/{short}",
    "refs/remotes/{short}",
    "refs/remotes/{short}/HEAD",
];

const NUM_REV_PARSE_RULES: usize = REV_PARSE_RULES.len();

/// Match `full_ref` against `rule` and return the embedded short segment when it fits.
fn match_parse_rule<'a>(full_ref: &'a str, rule: &str) -> Option<&'a str> {
    let (prefix, suffix) = rule.split_once("{short}")?;
    if !full_ref.starts_with(prefix) {
        return None;
    }
    let rest = &full_ref[prefix.len()..];
    if suffix.is_empty() {
        return Some(rest);
    }
    rest.strip_suffix(suffix)
}

fn expand_rule(rule_index: usize, short: &str) -> String {
    REV_PARSE_RULES[rule_index].replace("{short}", short)
}

fn ref_exists(repo: &Repository, name: &str) -> Result<bool> {
    Ok(repo.refs().read_raw(name)?.is_some())
}

/// Follow symbolic refs from `start` to the terminal ref name Git uses for DWIM output.
fn peel_symref_to_terminal_ref(repo: &Repository, start: &str) -> Result<String> {
    let refname_opts = RefNameOptions::default();
    let mut name = start.to_owned();
    for _ in 0..SYMREF_MAXDEPTH {
        let Some(raw) = repo.refs().read_raw(&name)? else {
            return Ok(name);
        };
        match raw {
            RawRef::Direct(_) => return Ok(name),
            RawRef::Symbolic(target) => {
                if check_refname_format(&target, &refname_opts).is_err() {
                    return Ok(name);
                }
                name = target;
            }
        }
    }
    Err(Error::RefStore(crate::refs::store::RefStoreError::SymrefLoop))
}

fn symref_target_resolves(repo: &Repository, start: &str) -> Result<bool> {
    let terminal = peel_symref_to_terminal_ref(repo, start)?;
    Ok(repo.resolve_ref_oid(&terminal).is_ok())
}

/// Read `core.warnAmbiguousRefs` (default **true** / strict shortening).
#[must_use]
pub fn warn_ambiguous_refs_from_config(repo: &Repository) -> bool {
    let Ok(config) = repo.config() else {
        return true;
    };
    config
        .get_bool("core.warnambiguousrefs")
        .or_else(|| config.get_bool("core.warnAmbiguousRefs"))
        .and_then(|r| r.ok())
        .unwrap_or(true)
}

/// Expand `spec` with Git `expand_ref` / `repo_dwim_ref` rules.
///
/// Returns how many rules matched and the first matching ref name (the rule's candidate).
///
/// # Errors
///
/// Propagates ref-store read failures.
pub fn expand_ref_dwim_full_name(repo: &Repository, spec: &str) -> Result<(usize, Option<String>)> {
    let warn = warn_ambiguous_refs_from_config(repo);
    let mut count = 0usize;
    let mut first = None;
    for rule in DWIM_RULES {
        let candidate = rule.replace("{0}", spec);
        let Some(raw) = repo.refs().read_raw(&candidate)? else {
            continue;
        };
        let valid = match raw {
            RawRef::Direct(_) => true,
            RawRef::Symbolic(_) => symref_target_resolves(repo, &candidate)?,
        };
        if valid {
            count += 1;
            if first.is_none() {
                first = Some(peel_symref_to_terminal_ref(repo, &candidate)?);
            }
            if !warn {
                break;
            }
        } else if matches!(raw, RawRef::Symbolic(_)) && candidate != "HEAD" {
            repo.warn(Warning::DanglingSymref { name: candidate });
        }
    }
    Ok((count, first))
}

/// Shorten `full_ref` to the longest DWIM form that still resolves only to this ref.
///
/// When `strict` is true (Git default via `core.warnAmbiguousRefs`), every other rev-parse
/// rule must fail to match an existing ref. When `strict` is false, only higher-priority
/// rules (lower indices) are considered.
///
/// # Errors
///
/// Propagates config and ref-store errors from existence checks.
pub fn shorten_unambiguous(repo: &Repository, full_ref: &str, strict: bool) -> Result<String> {
    // Rule 0 always matches; start from the most specific rules.
    for i in (1..NUM_REV_PARSE_RULES).rev() {
        let Some(short) = match_parse_rule(full_ref, REV_PARSE_RULES[i]) else {
            continue;
        };
        let rules_to_fail = if strict { NUM_REV_PARSE_RULES } else { i };
        let mut ambiguous = false;
        for j in 0..rules_to_fail {
            if j == i {
                continue;
            }
            let candidate = expand_rule(j, short);
            if ref_exists(repo, &candidate)? {
                ambiguous = true;
                break;
            }
        }
        if !ambiguous {
            return Ok(short.to_owned());
        }
    }
    Ok(full_ref.to_owned())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::refs::{write_ref, write_symbolic_ref};
    use crate::repo::{InitOptions, Repository};

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let oid = crate::objects::ObjectId::from_hex("0000000000000000000000000000000000000001")
            .expect("oid");
        write_ref(repo.git_dir(), "refs/heads/main", &oid).expect("main");
        write_ref(repo.git_dir(), "HEAD", &oid).expect("head");
        (dir, repo)
    }

    fn tip(repo: &Repository) -> crate::objects::ObjectId {
        repo.resolve_ref_oid("refs/heads/main").expect("main")
    }

    #[test]
    fn shorten_branch_and_tag() {
        let (_dir, repo) = init_repo();
        let head = tip(&repo);
        write_ref(repo.git_dir(), "refs/heads/feature/x", &head).expect("write");
        write_ref(repo.git_dir(), "refs/tags/feature/x", &head).expect("write tag");
        let short = shorten_unambiguous(&repo, "refs/heads/feature/x", true).expect("shorten");
        assert_eq!(short, "heads/feature/x");
        let tag_short = shorten_unambiguous(&repo, "refs/tags/feature/x", true).expect("tag");
        assert_eq!(tag_short, "tags/feature/x");
    }

    #[test]
    fn ambiguous_short_name_keeps_prefix() {
        let (_dir, repo) = init_repo();
        let head = tip(&repo);
        write_ref(repo.git_dir(), "refs/heads/x", &head).expect("branch");
        write_ref(repo.git_dir(), "refs/tags/x", &head).expect("tag");
        let branch = shorten_unambiguous(&repo, "refs/heads/x", true).expect("branch");
        assert_eq!(branch, "heads/x");
        let tag = shorten_unambiguous(&repo, "refs/tags/x", true).expect("tag");
        assert_eq!(tag, "tags/x");
    }

    #[test]
    fn loose_mode_shortens_tag_with_warn_ambiguous_refs_false() {
        let (_dir, repo) = init_repo();
        let head = tip(&repo);
        write_ref(repo.git_dir(), "refs/heads/x", &head).expect("branch");
        write_ref(repo.git_dir(), "refs/tags/x", &head).expect("tag");
        let config_path = repo.common_git_dir().join("config");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config_path)
            .expect("open config")
            .write_all(b"\n[core]\n\twarnAmbiguousRefs = FALSE\n")
            .expect("write config");
        let worktree = repo.git_dir().parent().expect("worktree");
        let repo = Repository::discover(
            Some(worktree),
            &crate::config::environment::RepositoryOptions::empty(),
        )
        .expect("reopen");
        assert!(!warn_ambiguous_refs_from_config(&repo));
        let tag = shorten_unambiguous(&repo, "refs/tags/x", false)
        .expect("tag");
        assert_eq!(tag, "x");
    }

    #[test]
    fn peel_symref_chain_to_terminal_branch() {
        let (_dir, repo) = init_repo();
        write_symbolic_ref(repo.git_dir(), "refs/heads/alias", "refs/heads/main").expect("alias");
        write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/alias").expect("head");
        let (count, full) = expand_ref_dwim_full_name(&repo, "HEAD").expect("expand");
        assert_eq!(count, 1);
        assert_eq!(full.as_deref(), Some("refs/heads/main"));
    }
}
