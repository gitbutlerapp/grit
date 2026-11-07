---
title: Revwalk
summary: Resolve revisions, walk history with rev-list, ranges (A..B), ordering, and merge bases.
---

History walks start from one or more **tips** (resolved ref names or expressions), follow parent links, optionally subtract another set of tips, then sort the result. Everything that needs a repository is a method on [`RevWalk`](rustdoc:grit_lib::revwalk::RevWalk), the handle [`Repository::revwalk`](rustdoc:grit_lib::repo::Repository::revwalk) returns: resolving revisions, listing and counting commits, merge bases and reachability, log decorations and author grouping. The `revwalk::rev_parse` and `revwalk::rev_list` modules keep only the option and result types and pure helpers that parse strings.

## Rev-parse

[`RevWalk::resolve`](rustdoc:grit_lib::revwalk::RevWalk::resolve) turns a single spec (`HEAD`, `main`, `v1.0`, `abc1234`, `main^`, `HEAD~3`, `HEAD:path`, `@{-1}`, `:/message`, peel suffixes and more) into an [`ObjectId`](rustdoc:grit_lib::objects::ObjectId). [`ResolveOptions`](rustdoc:grit_lib::revwalk::ResolveOptions) says how:

| Field | Values | Effect |
| --- | --- | --- |
| [`peel`](rustdoc:grit_lib::revwalk::Peel) | `Any` (default), `Commit`, `Tree`, `Blob` | The kind the result is peeled to. The kind also breaks ties when abbreviated hex matches several objects. `Commit` reads `A..B` as `B` and `A...B` as the merge base, like the commit-ish arguments of `log` or `reset`. |
| [`index_dwim`](rustdoc:grit_lib::revwalk::IndexDwim) | `On` (default), `Off` | Whether a name that matches no ref or object is reported as a possible path (`RevParseError::AmbiguousArgument`) or as a bad revision (`RevParseError::InvalidObjectName`). |
| [`purpose`](rustdoc:grit_lib::revwalk::ResolvePurpose) | `Revision` (default), `RangeEnd`, `CheckoutNoGuess` | `RangeEnd` makes ambiguous abbreviated hex prefer a commit, as range endpoints and `rev-parse --verify` do. `CheckoutNoGuess` stops a bare name from matching the one remote-tracking branch of that name (`checkout --no-guess`). |

```rust
use grit_lib::revwalk::{IndexDwim, Peel, ResolveOptions, ResolvePurpose};

# fn demo(repo: &grit_lib::repo::Repository) -> grit_lib::error::Result<()> {
let revwalk = repo.revwalk();
// git rev-parse --verify <spec>
let verify = ResolveOptions::default()
    .with_index_dwim(IndexDwim::Off)
    .with_purpose(ResolvePurpose::RangeEnd);
let head = revwalk.resolve("HEAD", &verify)?;
// The commit and the root tree of a tag
let commit = revwalk.resolve("v1.0", &ResolveOptions::default().with_peel(Peel::Commit))?;
let tree = revwalk.resolve("v1.0", &ResolveOptions::default().with_peel(Peel::Tree))?;
# let _ = (head, commit, tree);
# Ok(())
# }
```

The same handle peels ids you already hold ([`peel_to_commit`](rustdoc:grit_lib::revwalk::RevWalk::peel_to_commit), [`peel_to_tree`](rustdoc:grit_lib::revwalk::RevWalk::peel_to_tree)), names refs ([`symbolic_full_name`](rustdoc:grit_lib::revwalk::RevWalk::symbolic_full_name), [`upstream_symbolic_name`](rustdoc:grit_lib::revwalk::RevWalk::upstream_symbolic_name) for `@{upstream}` and `@{push}`), and shortens ids ([`abbreviate`](rustdoc:grit_lib::revwalk::RevWalk::abbreviate) with an [`AbbrevLength`](rustdoc:grit_lib::revwalk::AbbrevLength): `core.abbrev` or a minimum length; [`abbrev_matches`](rustdoc:grit_lib::revwalk::RevWalk::abbrev_matches) lists every object a prefix names).

Common helpers that do not need a repository:

- [`split_double_dot_range`](rustdoc:grit_lib::revwalk::rev_parse::split_double_dot_range) — split `main..feature` into two tokens (ignores `...` and path segments).
- [`abbreviate_ref_name`](rustdoc:grit_lib::revwalk::rev_parse::abbreviate_ref_name) — shorten `refs/heads/main` to `main` for display.

## Listing commits on a repository handle

`RevWalk` has two ways to list commits:

- [`RevWalk::commits`](rustdoc:grit_lib::revwalk::RevWalk::commits), [`rev_list_result`](rustdoc:grit_lib::revwalk::RevWalk::rev_list_result) and [`count`](rustdoc:grit_lib::revwalk::RevWalk::count) take revision **tokens** (`&["main", "^v1.0"]`, `&["main...topic"]`) plus [`RevWalkListOptions`](rustdoc:grit_lib::revwalk::RevWalkListOptions). This is the API most callers want. Ref selections (`--all`, `--branches`) go in `RevWalkListOptions::refs` and path limits in [`RevWalkListOptions::pathspec`](rustdoc:grit_lib::revwalk::RevWalkListOptions::pathspec) (default [`Pathspec::entire_tree`](rustdoc:grit_lib::worktree::pathspec::Pathspec::entire_tree)), not in the tokens. Build that value with [`Repository::pathspec`](rustdoc:grit_lib::repo::Repository::pathspec) or [`Pathspec::scoped`](rustdoc:grit_lib::worktree::pathspec::Pathspec::scoped).
- For plain default-order walks (no path limits, filters, or symmetric diff markers), [`RevWalk::commits_stream`](rustdoc:grit_lib::revwalk::RevWalk::commits_stream) yields commits lazily with the same tokens and options, optionally retaining raw commit bytes the walk already read (`RevWalkListOptions::retain_commit_data`). Set `RevWalkListOptions::retained_commit_extent` to [`CommitExtent::ThroughSubject`](rustdoc:grit_lib::revwalk::CommitExtent) when the listing shows only one line per commit (author, dates, change id, subject): the walk then reads each commit only through its subject line instead of inflating its whole message, and the retained bytes are that prefix (see [`Odb::read_commit_through_subject`](rustdoc:grit_lib::objects::odb::Odb::read_commit_through_subject)). `grit log` and `grit revs` do this for human and Markdown output; JSON rows carry the full message and keep [`CommitExtent::Whole`](rustdoc:grit_lib::revwalk::CommitExtent).
- [`RevWalk::walk`](rustdoc:grit_lib::revwalk::RevWalk::walk) takes **one** spec (`main`, `^tip`, `A..B` or `A...B`) plus the full engine [`RevListOptions`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) described below. Options, `--`, and several space-separated tokens fail with `RevListError::UnsupportedWalkSpec`, so use `commits` for those.

Both list the symmetric difference for `A...B`, exactly like `git rev-list A...B`. [`RevWalk::parse_tokens`](rustdoc:grit_lib::revwalk::RevWalk::parse_tokens) does the expansion if you call [`RevWalk::rev_list`](rustdoc:grit_lib::revwalk::RevWalk::rev_list) yourself.

When expanding symmetric ranges on ordinary histories, endpoint navigation such as
`HEAD~500` shares the parent links and timestamps it parses with the following walk.
Standalone revision resolution does not retain this metadata. Navigation seeds at most 1,024
entries, so resolving a distant ancestor alone does not retain its entire history.
Shallow boundaries, grafts, promisor repositories and replacement objects keep their
existing traversal rules.

## Rev-list

[`RevWalk::rev_list`](rustdoc:grit_lib::revwalk::RevWalk::rev_list) takes positive and negative revision specs plus [`RevListOptions`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions). It returns a [`RevListResult`](rustdoc:grit_lib::revwalk::rev_list::RevListResult) whose `commits` field is the final oid list (after skip, max-count, and reverse).

For counts, read [`RevListResult::commit_count`](rustdoc:grit_lib::revwalk::rev_list::RevListResult::commit_count) or use `RevWalk::count`: count-only walks can leave `commits` empty. For an unlimited count with only author, committer or message filters, the walk visits each reachable commit once, matches its borrowed bytes and discards the payload. It retains visited ids rather than parsed commit maps or a sorted list, including when subtracting excluded ancestry; shallow boundaries and grafts still apply.

| Option | Meaning |
| --- | --- |
| [`ordering`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) | [`OrderingMode`](rustdoc:grit_lib::revwalk::rev_list::OrderingMode) — default date order, topo, author-date variants. |
| `first_parent` | Follow only the first parent at merges. |
| `max_count` / `skip` | Limit how many commits are returned. |
| `reverse` | Reverse the selected list after sorting. |
| `commit_filter` | Optional [`CommitFilter`](rustdoc:grit_lib::revwalk::filter::CommitFilter) (`--author`, `--committer`, `--grep`, mailmap). |
| `pickaxe` | Optional [`Pickaxe`](rustdoc:grit_lib::diff::Pickaxe) (`-S` / `-G` tree-diff filters). |
| `retain_commit_data` | Keep the commits the walk parses in [`RevListResult::commit_data`](rustdoc:grit_lib::revwalk::rev_list::RevListResult), so code that prints the listed commits does not read them again. |

With [`RevListOptions::default`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) and [`OrderingMode::Default`](rustdoc:grit_lib::revwalk::rev_list::OrderingMode::Default), every listing path — bounded or unbounded [`max_count`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions::max_count), [`RevWalk::commits`](rustdoc:grit_lib::revwalk::RevWalk::commits), [`RevWalk::walk`](rustdoc:grit_lib::revwalk::RevWalk::walk), and [`RevWalk::commits_iter`](rustdoc:grit_lib::revwalk::RevWalk::commits_iter) — uses the same ungated committer-date priority queue as `git rev-list` (newest committer date first, equal dates broken by tip insertion order). When a limit is set and there are no negative specs, the walk stops as soon as it has enough commits. [`OrderingMode::DateOrderWalk`](rustdoc:grit_lib::revwalk::rev_list::OrderingMode::DateOrderWalk) is Git's `--date-order` variant: the same heap with a parent-after-children gate so skewed clocks do not print ancestors before descendants. The default enables the on-disk commit-graph for parent/date lookup when `core.commitGraph` allows it, matching `git rev-list` speed on large histories; set `use_commit_graph: false` to parse every commit object. A repository with a custom primary object store never reads graph files from `objects/`; its walks use the graph attached with [`OdbBuilder::commit_graph`](rustdoc:grit_lib::objects::odb::OdbBuilder::commit_graph) instead, built with [`Maintenance::build_commit_graph`](rustdoc:grit_lib::maintenance::Maintenance::build_commit_graph) (see [Object stores](object-stores.md#commit-graphs-for-a-custom-store)).

To print the listed commits, collect the ref labels once with [`RevWalk::decorations`](rustdoc:grit_lib::revwalk::RevWalk::decorations) ([`DecorationOptions`](rustdoc:grit_lib::porcelain::log::DecorationOptions) picks full or short names, the `--decorate-refs` filter, and optionally only the commits you show), set `retain_commit_data`, and hand `commit_data` to [`RevWalk::load_commits`](rustdoc:grit_lib::revwalk::RevWalk::load_commits). It moves out the commits the walk parsed and reads only the rest, such as commits whose parents came from a commit-graph; pass `&mut ()` when you hold none.

Path-limited walks compare the requested entries directly in tree data and reuse packed delta bases. At a merge, ordinary path history follows the first parent whose tree matches the requested paths. Setting `sparse: true` keeps every commit reached along those selected parent links, including merges whose paths match a parent. These rules work with SHA-1 and SHA-256 repositories, with or without commit-graph Bloom filters.

For a range `main..feature`, pass `feature` as a positive spec and `main` as a negative spec (or split with [`split_double_dot_range`](rustdoc:grit_lib::revwalk::rev_parse::split_double_dot_range) as the example does). A symmetric range `main...feature` needs every merge base, so expand it with `RevWalk::parse_tokens` and copy its `symmetric_left` / `symmetric_right` into the options; `rev_list` rejects a raw `A...B` spec with `RevListError::UnexpandedSymmetricRange`. That matches `git rev-list main..feature`, order included. In default order, with or without `max_count`, a range walk paints both sides down to their common frontier and lists only commits reachable from the positive tips but not from the excluded tips — the same set as `git rev-list`, not the whole history behind `main`. Committer dates order the paint heap but do not define membership; when generations are unavailable the paint may need an extra pass so hide-side commits with skewed dates still mark shared ancestors hidden. An expanded `main...feature` uses that same walk and stops at its merge bases, except with `OrderingMode::Topo` (`--graph`), which sorts the whole symmetric range. `OrderingMode::DateOrderWalk` (`--date-order`) and output filters such as paths or `--left-right` still sort the whole range first.

### Commit filters

[`CommitFilter`](rustdoc:grit_lib::revwalk::filter::CommitFilter) mirrors `git rev-list` / `git log` identity and message filtering. Build one with [`CommitFilterBuilder`](rustdoc:grit_lib::revwalk::filter::CommitFilterBuilder): author and committer patterns OR together; `--grep` patterns OR together unless `all_match` is set; `invert_grep` negates only the grep group; `-i` applies to all. Patterns default to Git’s POSIX BRE rules (use `fixed_strings` for literal `-F` mode). Load a [`MailmapTable`](rustdoc:grit_lib::config::mailmap::MailmapTable) with [`repo.config().mailmap()`](rustdoc:grit_lib::config::Config::mailmap) and pass it to the builder — the filter never reads config on its own. Set [`RevListOptions::commit_filter`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) or [`RevWalkListOptions::commit_filter`](rustdoc:grit_lib::revwalk::RevWalkListOptions); filtered commits stay traversable (like `--no-merges`), and `-n` / `--count` apply after filtering.

Bounded default-order filters inflate each visited commit once and reuse those bytes for traversal and matching; only the walk's queued frontier is held at a time. Set `retain_commit_data` when loading log rows: selected matches are returned in `RevListResult::commit_data`, including with a commit-graph, and can be passed to [`RevWalk::load_commits`](rustdoc:grit_lib::revwalk::RevWalk::load_commits). Skipped and nonmatching payloads are discarded, so selective filters do not retain every scanned message.

## Pickaxe (`-S` / `-G`)

[`Pickaxe`](rustdoc:grit_lib::diff::Pickaxe) filters commits by tree diffs against the first parent (Git `log -S` / `-G`). Build with [`Pickaxe::occurrences`](rustdoc:grit_lib::diff::Pickaxe::occurrences) or [`Pickaxe::line_grep`](rustdoc:grit_lib::diff::Pickaxe::line_grep), then set [`RevListOptions::pickaxe`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) or [`RevWalkListOptions::pickaxe`](rustdoc:grit_lib::revwalk::RevWalkListOptions). Evaluation runs after cheap [`CommitFilter`](rustdoc:grit_lib::revwalk::filter::CommitFilter) checks; [`RevWalkListOptions::pathspec`](rustdoc:grit_lib::revwalk::RevWalkListOptions::pathspec) limits which files are diffed.

<!-- include: grit-examples/src/bin/log_by_author.rs -->

## Merge base

[`RevWalk::merge_bases`](rustdoc:grit_lib::revwalk::RevWalk::merge_bases) takes the commits and a [`MergeBaseMode`](rustdoc:grit_lib::revwalk::MergeBaseMode): `Best` is `git merge-base A B...` (the best common ancestor of the first commit and a merge of the rest), `All` is `--all` and `Octopus` is `--octopus`. For diff-style “exactly one base or fail”, use [`merge_base_for_diff`](rustdoc:grit_lib::revwalk::RevWalk::merge_base_for_diff).

The other reachability questions are methods too:

| Method | Git equivalent |
| --- | --- |
| [`is_ancestor`](rustdoc:grit_lib::revwalk::RevWalk::is_ancestor) | `git merge-base --is-ancestor` |
| [`independent`](rustdoc:grit_lib::revwalk::RevWalk::independent) | `git merge-base --independent` |
| [`fork_point`](rustdoc:grit_lib::revwalk::RevWalk::fork_point) | `git merge-base --fork-point` (`None` when the upstream's reflog has no fork point) |
| [`ahead_behind`](rustdoc:grit_lib::revwalk::RevWalk::ahead_behind) | `git rev-list --left-right --count A...B`, with the newest commits of each side on request ([`AheadBehindOptions`](rustdoc:grit_lib::revwalk::AheadBehindOptions)) |
| [`reachable_excluding`](rustdoc:grit_lib::revwalk::RevWalk::reachable_excluding) | `git rev-list tips ^hide`, bounded and date-cut by [`ReachableWalkOptions`](rustdoc:grit_lib::revwalk::ReachableWalkOptions) |
| [`count_reachable_excluding`](rustdoc:grit_lib::revwalk::RevWalk::count_reachable_excluding) | `git rev-list --count tips ^hide` |

## Describe (nearest tag)

[`RevWalk::describe`](rustdoc:grit_lib::revwalk::RevWalk::describe) picks the reachable tag with the smallest exact range count from the tag’s peeled commit to the target revision ([`DescribeOptions`](rustdoc:grit_lib::revwalk::DescribeOptions), [`DescribedCommit`](rustdoc:grit_lib::revwalk::DescribedCommit)). The distance matches `grit revs --count tag..rev`, not necessarily Git’s lazy `git describe` depth. When commit-graph generations are available, one backward walk from the target discovers reachable tags (Git’s internal `describe` model): each commit is visited at most once, within flags propagate to parents, and the walk can stop early when the best candidate’s cone covers the queue. Walk depth picks the winner; the library runs one exact `peeled..rev` count for that tag only (not per candidate). Without a commit-graph, the same exact distances come from a single fixpoint pass over the reachable subgraph (within bits from tag peels, then one sweep)—not a separate rev-list count per tag. With `first_parent`, only the first parent is followed in either path.

## Example

The program below opens a repository (created by system Git in tests), walks `main..feature`, prints each commit oid, then prints the merge base of the range tips:

<!-- include: grit-examples/src/bin/guide_revwalk.rs -->

Run against a repo with diverged `main` and `feature` branches:

```bash
cargo run --bin guide_revwalk /path/to/repo main..feature
```

The integration test `guide_revwalk` checks commit order against `git rev-list` and the merge base against `git merge-base`.
