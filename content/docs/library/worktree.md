---
title: Worktree
summary: Linked worktrees, ignore rules, attributes, and pathspec helpers under the worktree domain.
---

Working-tree behavior lives under [`worktree`](rustdoc:grit_lib::worktree): ignore/attributes (`worktree::ignore`, `worktree::attributes`), pathspecs (`worktree::pathspec`), and checkout helpers. Porcelain commands (`status`, `add`, `checkout`, …) compose these modules with the index and ODB.

## Reftable interoperability

Linked worktree creation and branch switching use the repository's selected ref backend.
In a reftable repository, each worktree has its own `reftable/tables.list` stack for
`HEAD`, while branch tips remain in the shared stack. The admin directory's `HEAD`
file is a discovery placeholder; the symbolic branch or detached commit is stored
in reftable. Worktrees created by either grit or system Git can be reopened and
switched by the other tool without converting ref storage.

`AddLinkedWorktreeOptions::no_checkout` creates the same ref metadata while leaving
the index and tracked files absent. Git can resolve that worktree's `HEAD` immediately;
its status shows the missing tracked files until checkout materializes them.

### Add and switch benchmark

Local Linux x86_64 measurements against system Git 2.55.0, with release-profile
Grit builds before and after the HEAD fix. Each repository has 2,000 files, 2,000
refs and 31 commits. Add creates a detached worktree; switch alternates attached
branches across distinct commits in a Git-created linked worktree. Reftable stacks
are compacted before each build's run. Values are medians of 15 runs after two
warmups: wall milliseconds / peak RSS in KiB (`/usr/bin/time -v`). Compaction
compares `grit gc pack-refs` with `git pack-refs --all`.

| Operation | Git | Grit before | Grit after |
| --- | ---: | ---: | ---: |
| Files: add | 95.34 / 5,964 | 93.81 / 15,812 | 92.55 / 17,028 |
| Files: switch | 6.83 / 6,048 | 7.75 / 15,108 | 7.28 / 16,364 |
| Reftable: add | 98.57 / 5,980 | 93.51 / 16,224 | 94.87 / 17,192 |
| Reftable: switch | 7.69 / 6,332 | 12.18 / 15,672 | 11.89 / 16,932 |
| Reftable: compact | 2.33 / 4,540 | 9.57 / 13,048 | 9.41 / 13,384 |

The original reftable add/switch timings produced incorrect Git-visible HEAD
state. Correct initialization writes a per-worktree table, adding about 1.4 ms to
add in this fixture; switch and compaction are slightly faster. The release build's
measured peak RSS rises by about 1.0–1.2 MiB for add/switch and 0.3 MiB for compaction.

## Matching paths against pathspecs

To filter a path list by user-typed pathspecs, resolve each one with [`Repository::resolve_worktree_pathspec`](rustdoc:grit_lib::repo::Repository::resolve_worktree_pathspec), then build a [`PathspecMatcher`](rustdoc:grit_lib::pathspec::PathspecMatcher) once with [`Repository::pathspec_matcher`](rustdoc:grit_lib::repo::Repository::pathspec_matcher) and ask it about each path. It applies Git's list rules (any positive spec, no `:(exclude)` spec) and every magic word, including `:(attr:...)`. Attributes come from the repository's own `.gitattributes` stack (or `info/attributes` in a bare repository). `[attr]` macros are expanded, and `core.ignorecase` makes attribute patterns case-insensitive, as in Git. Attributes are only loaded when a spec asks for them, once per matcher rather than once per path. Building the matcher returns a `Result`: a malformed `attr:` clause, such as an empty `:(attr:)` or two `attr:` clauses in one spec, fails with [`Error::InvalidAttrPathspec`](rustdoc:grit_lib::error::Error::InvalidAttrPathspec) carrying an [`AttrPathspecError`](rustdoc:grit_lib::error::AttrPathspecError). `matches_change` also checks the old path of a rename. [`Repository::paths_match_pathspecs`](rustdoc:grit_lib::repo::Repository::paths_match_pathspecs) is the one-path shorthand. The free functions in `worktree::pathspec` (`matches_pathspec_list` and friends) have no attributes, so they never match a positive `:(attr:...)` spec. Use [`pathspec_has_attr_magic`](rustdoc:grit_lib::pathspec::pathspec_has_attr_magic) to detect those specs, or pass your own rules to [`matches_pathspec_list_for_object`](rustdoc:grit_lib::pathspec::matches_pathspec_list_for_object).

## Ignore rules

[`IgnoreMatcher::from_repository`](rustdoc:grit_lib::worktree::ignore::IgnoreMatcher::from_repository) loads `.git/info/exclude` and `core.excludesfile`, then reads each directory's `.gitignore` the first time a path under it is queried. It applies Git's precedence: a nested `.gitignore` beats its parents, and the last matching line wins. [`is_path_ignored`](rustdoc:grit_lib::worktree::ignore::IgnoreMatcher::is_path_ignored) answers yes or no for hot walks, and [`check_path`](rustdoc:grit_lib::worktree::ignore::IgnoreMatcher::check_path) also returns the deciding pattern. Status, staging, auto-stage commits and clean all use the same matcher. With `core.ignorecase=true`, patterns and nested `.gitignore` directories match without regard to ASCII case, as in Git: `Build/` ignores `build/` and `*.LOG` ignores `debug.log`. [`with_ignore_case`](rustdoc:grit_lib::worktree::ignore::IgnoreMatcher::with_ignore_case) overrides the setting. Tracked paths are also found without regard to case there, so status, clean and staging never treat `build/` as ignored while `Build/t` is tracked. The matcher's own tracked-path exemption stays exact-case, like `git check-ignore`.

## Linked worktrees (handle)

`Repository::worktrees` returns `RepositoryWorktrees`: list main and linked checkouts, add/remove/move/repair/lock linked worktrees with the same on-disk layout Git uses (`worktrees/` admin files, `gitdir` links). [`AddLinkedWorktreeOptions`](rustdoc:grit_lib::worktree::AddLinkedWorktreeOptions) uses [`WorktreeBranch`](rustdoc:grit_lib::worktree::WorktreeBranch) for branch modes (`Create`, `CreateOrReset`, `Existing`, `Detach`), optional lock-before-checkout, `--no-checkout`, and `--track`. [`move_linked_worktree`](rustdoc:grit_lib::worktree::move_linked_worktree) and [`repair_worktrees`](rustdoc:grit_lib::worktree::repair_worktrees) rewrite gitfile links atomically; repairs return [`WorktreeRepair`](rustdoc:grit_lib::worktree::WorktreeRepair) rows. [`prune_linked_worktrees`](rustdoc:grit_lib::worktree::prune_linked_worktrees) accepts [`PruneWorktreesOptions`](rustdoc:grit_lib::worktree::PruneWorktreesOptions) and returns [`PrunedWorktreeEntry`](rustdoc:grit_lib::worktree::PrunedWorktreeEntry) rows with a reason string.

Add, list and remove also work on a repository opened with an injected ref store or object store ([`RepositoryBuilder`](rustdoc:grit_lib::repo::RepositoryBuilder) or `with_ref_store`). Branches are created, read and rolled back in the injected store. The linked worktree's own `HEAD` lives in its admin directory (`worktrees/<id>/HEAD`), because the store holds only the main worktree's `HEAD`. Its branch tip is still resolved through the store, so `WorktreeEntry::head` matches what a files repository reports. Bareness comes from [`Repository::is_bare`](rustdoc:grit_lib::repo::Repository::is_bare), so a git directory kept outside the work tree is not mistaken for a bare repository.

Refusals are typed. Worktree failures arrive as [`Error::Worktree`](rustdoc:grit_lib::error::Error::Worktree) carrying a [`WorktreeError`](rustdoc:grit_lib::worktree::WorktreeError): `NotRegistered`, `MainWorktree` (with the refused [`WorktreeOperation`](rustdoc:grit_lib::worktree::WorktreeOperation)), `Locked`, `AlreadyLocked`, `NotLocked`, `HasLocalChanges`, `HasSubmodules`, `PathRegistered`, `PathRegisteredMissing`, `PathExists`, `BareRepository` and so on. Branch problems during add use the same [`BranchError`](rustdoc:grit_lib::porcelain::branch_error::BranchError) variants as branch creation (`InvalidName`, `NotFound`, `AlreadyExists`). A branch that is already checked out in another worktree is reported as `Error::BranchCheckedOutInWorktree`, and a start point that is not a commit as `Error::UnexpectedObjectKind`.

## Example

This program lists every worktree path grit sees for an open repository:

<!-- include: grit-examples/src/bin/guide_worktree.rs -->

Create a linked worktree with Git, then compare counts:

```bash
git worktree add ../linked -b feature
cargo run --bin guide_worktree .
git worktree list
```

The integration test adds a linked worktree with system Git and compares `worktree_count` to `git worktree list --porcelain`.
