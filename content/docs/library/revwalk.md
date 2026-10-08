---
title: Revwalk
summary: Resolve revisions, walk history with rev-list, ranges (A..B), ordering, and merge bases.
---

History walks start from one or more **tips** (resolved ref names or expressions), follow parent links, optionally subtract another set of tips, then sort the result. grit-lib exposes the same machinery the CLI uses through [`rev_parse`](rustdoc:grit_lib::rev_parse) and [`rev_list`](rustdoc:grit_lib::rev_list).

## Rev-parse

[`resolve_revision`](rustdoc:grit_lib::rev_parse::resolve_revision) turns a single spec (`HEAD`, `main`, `v1.0`, `abc1234`, `main^`, `HEAD~3`, tag peelers, and more) into an [`ObjectId`](rustdoc:grit_lib::objects::ObjectId). For range endpoints and log-style DWIM, [`resolve_revision_for_range_end`](rustdoc:grit_lib::rev_parse::resolve_revision_for_range_end) matches Git’s `A..B` left/right rules.

Common helpers that do not need a full repository walk:

- [`split_double_dot_range`](rustdoc:grit_lib::rev_parse::split_double_dot_range) — split `main..feature` into two tokens (ignores `...` and path segments).
- [`abbreviate_ref_name`](rustdoc:grit_lib::rev_parse::abbreviate_ref_name) — shorten `refs/heads/main` to `main` for display.

## Rev-list

[`rev_list`](rustdoc:grit_lib::rev_list::rev_list) takes positive and negative revision specs plus [`RevListOptions`](rustdoc:grit_lib::rev_list::RevListOptions). It returns a [`RevListResult`](rustdoc:grit_lib::rev_list::RevListResult) whose `commits` field is the final oid list (after skip, max-count, and reverse).

| Option | Meaning |
| --- | --- |
| [`ordering`](rustdoc:grit_lib::rev_list::RevListOptions) | [`OrderingMode`](rustdoc:grit_lib::rev_list::OrderingMode) — default date order, topo, author-date variants. |
| `first_parent` | Follow only the first parent at merges. |
| `max_count` / `skip` | Limit how many commits are returned. |
| `reverse` | Reverse the selected list after sorting. |

For a range `main..feature`, pass `feature` as a positive spec and `main` as a negative spec (or split with [`split_double_dot_range`](rustdoc:grit_lib::rev_parse::split_double_dot_range) as the example does). That matches `git rev-list main..feature`.

## Merge base

[`merge_bases_first_vs_rest`](rustdoc:grit_lib::merge_base::merge_bases_first_vs_rest) finds minimal common ancestors between one commit and one or more others — the library equivalent of `git merge-base A B`. For diff-style “pick exactly one base or fail”, see [`merge_base_for_diff_two_commits`](rustdoc:grit_lib::merge_base::merge_base_for_diff_two_commits).

## Example

The program below opens a repository (created by system Git in tests), walks `main..feature`, prints each commit oid, then prints the merge base of the range tips:

<!-- include: grit-examples/src/bin/guide_revwalk.rs -->

Run against a repo with diverged `main` and `feature` branches:

```bash
cargo run --bin guide_revwalk /path/to/repo main..feature
```

The integration test `guide_revwalk` checks commit order against `git rev-list` and the merge base against `git merge-base`.
