# Revwalk

> Resolve revisions, walk history with rev-list, ranges (A..B), ordering, and merge bases.

History walks start from one or more **tips** (resolved ref names or expressions), follow parent links, optionally subtract another set of tips, then sort the result. grit-lib exposes the same machinery the CLI uses through [`rev_parse`](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/index.html) and [`rev_list`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/index.html).

## Rev-parse

[`resolve_revision`](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/fn.resolve_revision.html) turns a single spec (`HEAD`, `main`, `v1.0`, `abc1234`, `main^`, `HEAD~3`, tag peelers, and more) into an [`ObjectId`](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.ObjectId.html). For range endpoints and log-style DWIM, [`resolve_revision_for_range_end`](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/fn.resolve_revision_for_range_end.html) matches Git’s `A..B` left/right rules.

Common helpers that do not need a full repository walk:

- [`split_double_dot_range`](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/fn.split_double_dot_range.html) — split `main..feature` into two tokens (ignores `...` and path segments).
- [`abbreviate_ref_name`](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/fn.abbreviate_ref_name.html) — shorten `refs/heads/main` to `main` for display.

## Rev-list

[`rev_list`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/fn.rev_list.html) takes positive and negative revision specs plus [`RevListOptions`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/struct.RevListOptions.html). It returns a [`RevListResult`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/struct.RevListResult.html) whose `commits` field is the final oid list (after skip, max-count, and reverse).

| Option | Meaning |
| --- | --- |
| [`ordering`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/struct.RevListOptions.html) | [`OrderingMode`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.OrderingMode.html) — default date order, topo, author-date variants. |
| `first_parent` | Follow only the first parent at merges. |
| `max_count` / `skip` | Limit how many commits are returned. |
| `reverse` | Reverse the selected list after sorting. |

For a range `main..feature`, pass `feature` as a positive spec and `main` as a negative spec (or split with [`split_double_dot_range`](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/fn.split_double_dot_range.html) as the example does). That matches `git rev-list main..feature`.

## Merge base

[`merge_bases_first_vs_rest`](https://docs.rs/grit-lib/latest/grit_lib/merge_base/fn.merge_bases_first_vs_rest.html) finds minimal common ancestors between one commit and one or more others — the library equivalent of `git merge-base A B`. For diff-style “pick exactly one base or fail”, see [`merge_base_for_diff_two_commits`](https://docs.rs/grit-lib/latest/grit_lib/merge_base/fn.merge_base_for_diff_two_commits.html).

## Example

The program below opens a repository (created by system Git in tests), walks `main..feature`, prints each commit oid, then prints the merge base of the range tips:

```rust
//! Walk commit history with [`grit_lib::rev_list::rev_list`] and compute a merge base.
//!
//! Source for the library guide "Revwalk" page (included in the docs site).

use grit_lib::merge_base::merge_bases_first_vs_rest;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};
use grit_lib::rev_parse::{resolve_revision, split_double_dot_range};
use std::path::Path;

fn main() -> Result<(), grit_lib::error::Error> {
    let mut args = std::env::args().skip(1);
    let repo_root = args.next().map(std::path::PathBuf::from).ok_or_else(|| {
        grit_lib::error::Error::Message("usage: guide_revwalk <repo> [main..feature]".to_owned())
    })?;
    let range = args.next().unwrap_or_else(|| "main..feature".to_owned());

    let repo = open_repo(&repo_root)?;
    let options = RevListOptions::default();
    let result = if let Some((left, right)) = split_double_dot_range(&range) {
        let mut positive = Vec::new();
        let mut negative = Vec::new();
        if !right.is_empty() {
            positive.push(right.to_owned());
        }
        if !left.is_empty() {
            negative.push(left.to_owned());
        }
        rev_list(&repo, &positive, &negative, &options)?
    } else {
        rev_list(&repo, std::slice::from_ref(&range), &[], &options)?
    };

    for oid in &result.commits {
        println!("{oid}");
    }

    let (left_name, right_name) = range_names(&range)?;
    let left = resolve_revision(&repo, left_name)?;
    let right = resolve_revision(&repo, right_name)?;
    let bases = merge_bases_first_vs_rest(&repo, left, &[right])?;
    let Some(base) = bases.first() else {
        return Err(grit_lib::error::Error::Message(
            "no merge base for range tips".to_owned(),
        ));
    };
    println!("merge_base={base}");

    Ok(())
}

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
    let git_dir = if root.join(".git").is_dir() {
        root.join(".git")
    } else {
        root.to_path_buf()
    };
    let work_tree = if root.join(".git").is_dir() {
        Some(root)
    } else {
        None
    };
    Repository::open(&git_dir, work_tree)
}

fn range_names(range: &str) -> Result<(&str, &str), grit_lib::error::Error> {
    split_double_dot_range(range).ok_or_else(|| {
        grit_lib::error::Error::Message(format!("expected a double-dot range, got {range:?}"))
    })
}
```

Run against a repo with diverged `main` and `feature` branches:

```
cargo run --bin guide_revwalk /path/to/repo main..feature
```

The integration test `guide_revwalk` checks commit order against `git rev-list` and the merge base against `git merge-base`.
