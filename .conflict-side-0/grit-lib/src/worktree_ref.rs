//! Per-worktree ref name parsing and storage location (Git `parse_worktree_ref` / `files_ref_path`).

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::refs::common_dir;

/// How a ref name maps to on-disk storage across worktrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefWorktreeType {
    /// `HEAD`, `refs/worktree/*`, `refs/bisect/*`, `refs/rewritten/*` in this checkout's git dir.
    Current,
    /// `main-worktree/HEAD` → common dir + bare name.
    Main,
    /// `worktrees/<id>/HEAD` → `common/worktrees/<id>/` + bare name.
    Other,
    /// Shared refs (`refs/heads/*`, …) in the common git directory.
    Shared,
}

/// `refs/worktree/*`, `refs/bisect/*`, and `refs/rewritten/*` are per-worktree.
#[must_use]
pub fn is_per_worktree_ref(refname: &str) -> bool {
    refname.starts_with("refs/worktree/")
        || refname.starts_with("refs/bisect/")
        || refname.starts_with("refs/rewritten/")
}

fn is_root_ref_syntax(refname: &str) -> bool {
    !refname.is_empty()
        && refname
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '-' || c == '_')
}

fn is_current_worktree_ref(refname: &str) -> bool {
    is_root_ref_syntax(refname) || is_per_worktree_ref(refname)
}

/// Parse `maybe_worktree_ref` like Git `parse_worktree_ref`.
///
/// Returns worktree kind, bare ref name for storage, and worktree id for `worktrees/<id>/…`.
#[must_use]
pub fn parse_worktree_ref(maybe: &str) -> (RefWorktreeType, Cow<'_, str>, Option<Cow<'_, str>>) {
    if let Some(rest) = maybe.strip_prefix("worktrees/") {
        if let Some((id, bare)) = rest.split_once('/') {
            if is_current_worktree_ref(bare) {
                return (
                    RefWorktreeType::Other,
                    Cow::Borrowed(bare),
                    Some(Cow::Borrowed(id)),
                );
            }
        }
        return (
            RefWorktreeType::Other,
            Cow::Borrowed(""),
            Some(Cow::Borrowed(rest)),
        );
    }

    if let Some(bare) = maybe.strip_prefix("main-worktree/") {
        if is_current_worktree_ref(bare) {
            return (RefWorktreeType::Main, Cow::Borrowed(bare), None);
        }
    }

    if is_current_worktree_ref(maybe) {
        return (RefWorktreeType::Current, Cow::Borrowed(maybe), None);
    }

    (RefWorktreeType::Shared, Cow::Borrowed(maybe), None)
}

/// Git directory and on-disk ref path for `refname` from the current process's linked checkout.
#[must_use]
pub fn resolve_ref_storage(git_dir: &Path, refname: &str) -> (PathBuf, String) {
    let common = common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
    let (kind, bare, wt_id) = parse_worktree_ref(refname);
    match kind {
        RefWorktreeType::Main => (common, bare.into_owned()),
        RefWorktreeType::Other => {
            let id = wt_id.map(|c| c.into_owned()).unwrap_or_default();
            (common.join("worktrees").join(id), bare.into_owned())
        }
        RefWorktreeType::Current => (git_dir.to_path_buf(), refname.to_owned()),
        RefWorktreeType::Shared => (common, refname.to_owned()),
    }
}

/// Whether `refname` should appear in `for-each-ref` from a linked worktree.
#[must_use]
pub fn ref_visible_from_worktree(git_dir: &Path, refname: &str) -> bool {
    if !is_per_worktree_ref(refname) {
        return true;
    }
    let (store, stor_name) = resolve_ref_storage(git_dir, refname);
    store.join(&stor_name).is_file()
}

/// True when `git_dir` is a linked worktree administrative directory.
#[must_use]
pub fn is_linked_worktree_git_dir(git_dir: &Path) -> bool {
    common_dir(git_dir).is_some()
}

/// DWIM rules matching Git `ref_rev_parse_rules` (`refs.c`).
const DWIM_RULES: &[&str] = &[
    "{0}",
    "refs/{0}",
    "refs/tags/{0}",
    "refs/heads/{0}",
    "refs/remotes/{0}",
    "refs/remotes/{0}/HEAD",
];

/// Resolve `spec` using Git `ref_rev_parse_rules` / `expand_ref`.
///
/// Returns the number of matching rules and the OID from the first match.
pub fn resolve_ref_dwim<F>(mut resolve: F, spec: &str) -> (usize, Option<crate::objects::ObjectId>)
where
    F: FnMut(&str) -> Option<crate::objects::ObjectId>,
{
    let mut count = 0usize;
    let mut first = None;
    for rule in DWIM_RULES {
        let candidate = rule.replace("{0}", spec);
        if let Some(oid) = resolve(&candidate) {
            count += 1;
            if first.is_none() {
                first = Some(oid);
            }
        }
    }
    (count, first)
}

/// When exactly one DWIM ref rule matches `spec`, return that expanded ref name.
///
/// Rule order matches Git (`refs/tags` before `refs/heads`). Returns `None` when zero or
/// more than one rule matches.
#[must_use]
pub fn sole_dwim_ref_name<F>(mut exists: F, spec: &str) -> Option<String>
where
    F: FnMut(&str) -> bool,
{
    let mut count = 0usize;
    let mut sole = None;
    for rule in DWIM_RULES {
        let candidate = rule.replace("{0}", spec);
        if exists(&candidate) {
            count += 1;
            if sole.is_none() {
                sole = Some(candidate);
            }
        }
    }
    if count == 1 {
        sole
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectId;
    use std::borrow::Cow;

    #[test]
    fn parse_worktree_ref_prefixes() {
        let (kind, bare, id) = parse_worktree_ref("main-worktree/HEAD");
        assert_eq!(kind, RefWorktreeType::Main);
        assert_eq!(bare, "HEAD");
        assert!(id.is_none());

        let (kind, bare, id) = parse_worktree_ref("worktrees/wt1/HEAD");
        assert_eq!(kind, RefWorktreeType::Other);
        assert_eq!(bare, "HEAD");
        assert_eq!(id, Some(Cow::Borrowed("wt1")));

        let (kind, bare, _) = parse_worktree_ref("refs/heads/main");
        assert_eq!(kind, RefWorktreeType::Shared);
        assert_eq!(bare, "refs/heads/main");
    }

    #[test]
    fn is_per_worktree_ref_matches_git_prefixes() {
        assert!(is_per_worktree_ref("refs/worktree/foo"));
        assert!(is_per_worktree_ref("refs/bisect/foo"));
        assert!(!is_per_worktree_ref("refs/heads/main"));
    }

    #[test]
    fn resolve_ref_dwim_finds_head_alias() {
        let oid = ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap();
        let resolve = |name: &str| {
            if name == "refs/heads/main" {
                Some(oid)
            } else {
                None
            }
        };
        let (count, first) = resolve_ref_dwim(resolve, "main");
        assert!(count >= 1);
        assert_eq!(first, Some(oid));
    }

    #[test]
    fn ref_visible_from_worktree_per_worktree_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("refs/worktree")).unwrap();
        std::fs::write(git_dir.join("refs/worktree/x"), b"ref: refs/heads/main\n").unwrap();
        assert!(ref_visible_from_worktree(&git_dir, "refs/worktree/x"));
        assert!(!ref_visible_from_worktree(
            &git_dir,
            "refs/worktree/missing"
        ));
    }
}
