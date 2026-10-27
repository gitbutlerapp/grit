---
title: Revwalk
summary: Resolve revisions, walk history with rev-list, ranges (A..B), ordering, and merge bases.
---

History walks start from one or more **tips** (resolved ref names or expressions), follow parent links, optionally subtract another set of tips, then sort the result. Prefer `Repository::rev_walk` (`RevWalk`) for resolve/list on an open handle; free functions live under `revwalk::rev_parse` and `revwalk::rev_list`.

## Rev-parse

[`resolve_revision`](rustdoc:grit_lib::revwalk::rev_parse::resolve_revision) turns a single spec (`HEAD`, `main`, `v1.0`, `abc1234`, `main^`, `HEAD~3`, tag peelers, and more) into an [`ObjectId`](rustdoc:grit_lib::objects::ObjectId). For range endpoints and log-style DWIM, [`resolve_revision_for_range_end`](rustdoc:grit_lib::revwalk::rev_parse::resolve_revision_for_range_end) matches Git’s `A..B` left/right rules.

Common helpers that do not need a full repository walk:

- [`split_double_dot_range`](rustdoc:grit_lib::revwalk::rev_parse::split_double_dot_range) — split `main..feature` into two tokens (ignores `...` and path segments).
- [`abbreviate_ref_name`](rustdoc:grit_lib::revwalk::rev_parse::abbreviate_ref_name) — shorten `refs/heads/main` to `main` for display.

## Listing commits on a repository handle

`RevWalk` has two ways to list commits:

- [`RevWalk::commits`](rustdoc:grit_lib::revwalk::RevWalk::commits), [`rev_list_result`](rustdoc:grit_lib::revwalk::RevWalk::rev_list_result) and [`count`](rustdoc:grit_lib::revwalk::RevWalk::count) take revision **tokens** (`&["main", "^v1.0"]`, `&["main...topic"]`) plus [`RevWalkListOptions`](rustdoc:grit_lib::revwalk::queries::RevWalkListOptions). This is the API most callers want. Ref selections (`--all`, `--branches`) go in `RevWalkListOptions::refs` and path limits in `RevWalkListOptions::paths`, not in the tokens.
- [`RevWalk::walk`](rustdoc:grit_lib::revwalk::RevWalk::walk) takes **one** spec (`main`, `^tip`, `A..B` or `A...B`) plus the full engine [`RevListOptions`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) described below. Options, `--`, and several space-separated tokens fail with `RevListError::UnsupportedWalkSpec`, so use `commits` for those.

Both list the symmetric difference for `A...B`, exactly like `git rev-list A...B`. [`parse_rev_walk_tokens`](rustdoc:grit_lib::revwalk::parse_rev_walk_tokens) does the expansion if you call `rev_list` yourself.

When expanding symmetric ranges on ordinary histories, endpoint navigation such as
`HEAD~500` shares the parent links and timestamps it parses with the following walk.
Standalone revision resolution does not retain this metadata. Navigation seeds at most 1,024
entries, so resolving a distant ancestor alone does not retain its entire history.
Shallow boundaries, grafts, promisor repositories and replacement objects keep their
existing traversal rules.

## Rev-list

[`rev_list`](rustdoc:grit_lib::revwalk::rev_list::rev_list) takes positive and negative revision specs plus [`RevListOptions`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions). It returns a [`RevListResult`](rustdoc:grit_lib::revwalk::rev_list::RevListResult) whose `commits` field is the final oid list (after skip, max-count, and reverse).

For counts, read [`RevListResult::commit_count`](rustdoc:grit_lib::revwalk::rev_list::RevListResult::commit_count) or use `RevWalk::count`: count-only walks can leave `commits` empty. For an unlimited count with only author, committer or message filters, the walk visits each reachable commit once, matches its borrowed bytes and discards the payload. It retains visited ids rather than parsed commit maps or a sorted list, including when subtracting excluded ancestry; shallow boundaries and grafts still apply.

| Option | Meaning |
| --- | --- |
| [`ordering`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) | [`OrderingMode`](rustdoc:grit_lib::revwalk::rev_list::OrderingMode) — default date order, topo, author-date variants. |
| `first_parent` | Follow only the first parent at merges. |
| `max_count` / `skip` | Limit how many commits are returned. |
| `reverse` | Reverse the selected list after sorting. |
| `commit_filter` | Optional [`CommitFilter`](rustdoc:grit_lib::revwalk::filter::CommitFilter) (`--author`, `--committer`, `--grep`, mailmap). |
| `pickaxe` | Optional [`Pickaxe`](rustdoc:grit_lib::diff::pickaxe::Pickaxe) (`-S` / `-G` tree-diff filters). |
| `retain_commit_data` | Keep the commits the walk parses in [`RevListResult::commit_data`](rustdoc:grit_lib::revwalk::rev_list::RevListResult), so code that prints the listed commits does not read them again. |

With [`RevListOptions::default`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) and default date order, `max_count` and no negative specs, the walk stops as soon as it has enough commits, however large the limit, and lists them in exactly `git rev-list` order (newest committer date first, ties in discovery order). The default enables the on-disk commit-graph for parent/date lookup when `core.commitGraph` allows it, matching `git rev-list` speed on large histories; set `use_commit_graph: false` to parse every commit object.

To print the listed commits, set `retain_commit_data` and hand `commit_data` to [`load_commits_with_decorations_reusing`](rustdoc:grit_lib::porcelain::log::load_commits_with_decorations_reusing). It moves out the commits the walk parsed and reads only the rest, such as commits whose parents came from a commit-graph.

Path-limited walks compare the requested entries directly in tree data and reuse packed delta bases. At a merge, ordinary path history follows the first parent whose tree matches the requested paths. Setting `sparse: true` keeps every commit reached along those selected parent links, including merges whose paths match a parent. These rules work with SHA-1 and SHA-256 repositories, with or without commit-graph Bloom filters.

For a range `main..feature`, pass `feature` as a positive spec and `main` as a negative spec (or split with [`split_double_dot_range`](rustdoc:grit_lib::revwalk::rev_parse::split_double_dot_range) as the example does). A symmetric range `main...feature` needs every merge base, so expand it with `parse_rev_walk_tokens` and copy its `symmetric_left` / `symmetric_right` into the options; `rev_list` rejects a raw `A...B` spec with `RevListError::UnexpandedSymmetricRange`. That matches `git rev-list main..feature`, order included. In default order, with or without `max_count`, a range walk only visits commits down to where the two sides meet, not the whole history behind `main`, so a short topic range on a large repository is as quick as `git rev-list`. An expanded `main...feature` uses that same walk and stops at its merge bases, except with `OrderingMode::Topo` (`--graph`), which sorts the whole symmetric range. `OrderingMode::DateOrderWalk` (`--date-order`) and output filters such as paths or `--left-right` still sort the whole range first.

### Commit filters

[`CommitFilter`](rustdoc:grit_lib::revwalk::filter::CommitFilter) mirrors `git rev-list` / `git log` identity and message filtering. Build one with [`CommitFilterBuilder`](rustdoc:grit_lib::revwalk::filter::CommitFilterBuilder): author and committer patterns OR together; `--grep` patterns OR together unless `all_match` is set; `invert_grep` negates only the grep group; `-i` applies to all. Patterns default to Git’s POSIX BRE rules (use `fixed_strings` for literal `-F` mode). Load a [`MailmapTable`](rustdoc:grit_lib::config::mailmap::MailmapTable) with [`load_mailmap_table`](rustdoc:grit_lib::config::mailmap::load_mailmap_table) and pass it to the builder — the filter never reads config on its own. Set [`RevListOptions::commit_filter`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) or [`RevWalkListOptions::commit_filter`](rustdoc:grit_lib::revwalk::queries::RevWalkListOptions); filtered commits stay traversable (like `--no-merges`), and `-n` / `--count` apply after filtering.

Bounded default-order filters inflate each visited commit once and reuse those bytes for traversal and matching; only the walk's queued frontier is held at a time. Set `retain_commit_data` when loading log rows: selected matches are returned in `RevListResult::commit_data`, including with a commit-graph, and can be passed to [`load_commits_with_decorations_reusing`](rustdoc:grit_lib::porcelain::log::load_commits_with_decorations_reusing). Skipped and nonmatching payloads are discarded, so selective filters do not retain every scanned message.

## Pickaxe (`-S` / `-G`)

[`Pickaxe`](rustdoc:grit_lib::diff::pickaxe::Pickaxe) filters commits by tree diffs against the first parent (Git `log -S` / `-G`). Build with [`Pickaxe::occurrences`](rustdoc:grit_lib::diff::pickaxe::Pickaxe::occurrences) or [`Pickaxe::line_grep`](rustdoc:grit_lib::diff::pickaxe::Pickaxe::line_grep), then set [`RevListOptions::pickaxe`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) or [`RevWalkListOptions::pickaxe`](rustdoc:grit_lib::revwalk::queries::RevWalkListOptions). Evaluation runs after cheap [`CommitFilter`](rustdoc:grit_lib::revwalk::filter::CommitFilter) checks; pathspecs limit which files are diffed.

<!-- include: grit-examples/src/bin/log_by_author.rs -->

## Merge base

[`merge_bases_first_vs_rest`](rustdoc:grit_lib::revwalk::merge_base::merge_bases_first_vs_rest) finds minimal common ancestors between one commit and one or more others — the library equivalent of `git merge-base A B`. For diff-style “pick exactly one base or fail”, see [`merge_base_for_diff_two_commits`](rustdoc:grit_lib::revwalk::merge_base::merge_base_for_diff_two_commits).

## Example

The program below opens a repository (created by system Git in tests), walks `main..feature`, prints each commit oid, then prints the merge base of the range tips:

<!-- include: grit-examples/src/bin/guide_revwalk.rs -->

Run against a repo with diverged `main` and `feature` branches:

```bash
cargo run --bin guide_revwalk /path/to/repo main..feature
```

The integration test `guide_revwalk` checks commit order against `git rev-list` and the merge base against `git merge-base`.
