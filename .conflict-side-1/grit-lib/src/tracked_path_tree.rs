//! Index-driven path-tree helpers for skipping redundant directory enumeration.
//!
//! When every tracked path under a directory shares the same next component (a linear
//! chain of intermediate directories), status and untracked walks can descend directly
//! without listing the directory on disk.

#[cfg(test)]
use std::collections::BTreeSet;

#[cfg(test)]
/// Prefix for paths strictly under `rel_dir` (`""` when `rel_dir` is empty).
#[must_use]
pub(crate) fn directory_prefix(rel_dir: &str) -> String {
    if rel_dir.is_empty() {
        String::new()
    } else {
        format!("{rel_dir}/")
    }
}

#[cfg(test)]
/// Returns the sole next path component when every indexed path under `rel_dir` continues
/// through the same subdirectory and none of them is a file directly in `rel_dir`.
#[must_use]
pub(crate) fn unique_indexed_subdirectory(
    tracked: &BTreeSet<String>,
    gitlinks: &BTreeSet<String>,
    rel_dir: &str,
) -> Option<String> {
    let prefix = directory_prefix(rel_dir);
    let mut component: Option<String> = None;
    let mut direct_file = false;

    for path in tracked.range(prefix.clone()..) {
        if !path.starts_with(&prefix) {
            break;
        }
        let rest = path.strip_prefix(&prefix).unwrap_or("");
        if rest.is_empty() {
            continue;
        }
        match rest.split_once('/') {
            Some((first, _)) => match &component {
                None => component = Some(first.to_owned()),
                Some(c) if c == first => {}
                _ => return None,
            },
            None => {
                direct_file = true;
            }
        }
    }

    for g in gitlinks {
        if g.as_str() == rel_dir {
            continue;
        }
        if !g.starts_with(&prefix) {
            continue;
        }
        let rest = g.strip_prefix(&prefix).unwrap_or("");
        if rest.is_empty() {
            continue;
        }
        let first = rest.split('/').next().unwrap_or("");
        if first.is_empty() {
            continue;
        }
        match &component {
            None => component = Some(first.to_owned()),
            Some(c) if c == first => {}
            _ => return None,
        }
    }

    if direct_file {
        None
    } else {
        component
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_subdir_for_linear_chain() {
        let tracked: BTreeSet<String> = ["a/b/c/file.txt".to_owned()].into_iter().collect();
        let gitlinks = BTreeSet::new();
        assert_eq!(
            unique_indexed_subdirectory(&tracked, &gitlinks, ""),
            Some("a".to_owned())
        );
        assert_eq!(
            unique_indexed_subdirectory(&tracked, &gitlinks, "a"),
            Some("b".to_owned())
        );
        assert_eq!(
            unique_indexed_subdirectory(&tracked, &gitlinks, "a/b"),
            Some("c".to_owned())
        );
        assert_eq!(
            unique_indexed_subdirectory(&tracked, &gitlinks, "a/b/c"),
            None
        );
    }

    #[test]
    fn unique_subdir_none_when_sibling_files() {
        let tracked: BTreeSet<String> = ["a/x.txt".to_owned(), "a/y.txt".to_owned()]
            .into_iter()
            .collect();
        let gitlinks = BTreeSet::new();
        assert_eq!(unique_indexed_subdirectory(&tracked, &gitlinks, "a"), None);
    }
}
