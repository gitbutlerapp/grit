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

Build a [`Pathspec`](rustdoc:grit_lib::worktree::pathspec::Pathspec) once with [`Repository::pathspec`](rustdoc:grit_lib::repo::Repository::pathspec) and [`PathspecOptions`](rustdoc:grit_lib::worktree::pathspec::PathspecOptions) (global flags from [`PathspecGlobalFlags::from_environment`](rustdoc:grit_lib::worktree::pathspec::PathspecGlobalFlags::from_environment), optional `--pathspec-from-file` via [`PathspecFile`](rustdoc:grit_lib::worktree::pathspec::PathspecFile)). Call [`Pathspec::matches`](rustdoc:grit_lib::worktree::pathspec::Pathspec::matches), [`matches_entry`](rustdoc:grit_lib::worktree::pathspec::Pathspec::matches_entry), or [`matches_change`](rustdoc:grit_lib::worktree::pathspec::Pathspec::matches_change) for each path. [`Pathspec::is_entire_tree`](rustdoc:grit_lib::worktree::pathspec::Pathspec::is_entire_tree) is true when no argv paths were given.

[`Repository::paths_match_pathspecs`](rustdoc:grit_lib::repo::Repository::paths_match_pathspecs) takes a [`Pathspec`](rustdoc:grit_lib::worktree::pathspec::Pathspec) and answers whether one repository-relative path matches. Attribute magic (`:(attr:...)`) is evaluated when you build the pathspec through the repository API; malformed `attr:` clauses fail with [`Error::InvalidAttrPathspec`](rustdoc:grit_lib::error::Error::InvalidAttrPathspec). [`Pathspec::matches_change`](rustdoc:grit_lib::worktree::pathspec::Pathspec::matches_change) also checks the old path of a rename. For argv-only helpers without a repository handle, [`Pathspec::scoped`](rustdoc:grit_lib::worktree::pathspec::Pathspec::scoped) and [`Pathspec::matches_path`](rustdoc:grit_lib::worktree::pathspec::Pathspec::matches_path) cover glob, exclude, icase, and literal magic; attribute specs still need a repository-built pathspec.

## Ignore rules

[`IgnoreMatcher::from_repository`](rustdoc:grit_lib::worktree::ignore::IgnoreMatcher::from_repository) loads `.git/info/exclude` and `core.excludesfile`, then reads each directory's `.gitignore` the first time a path under it is queried. It applies Git's precedence: a nested `.gitignore` beats its parents, and the last matching line wins. [`is_path_ignored`](rustdoc:grit_lib::worktree::ignore::IgnoreMatcher::is_path_ignored) answers yes or no for hot walks, and [`check_path`](rustdoc:grit_lib::worktree::ignore::IgnoreMatcher::check_path) also returns the deciding pattern. Status, staging, auto-stage commits and clean all use the same matcher. With `core.ignorecase=true`, patterns and nested `.gitignore` directories match without regard to ASCII case, as in Git: `Build/` ignores `build/` and `*.LOG` ignores `debug.log`. [`with_ignore_case`](rustdoc:grit_lib::worktree::ignore::IgnoreMatcher::with_ignore_case) overrides the setting. Tracked paths are also found without regard to case there, so status, clean and staging never treat `build/` as ignored while `Build/t` is tracked. The matcher's own tracked-path exemption stays exact-case, like `git check-ignore`.

## Quoted attribute patterns

Double-quoted patterns in `.gitattributes` accept C-style escapes for tabs, newlines,
quotes, backslashes and other control characters, plus three-digit octal byte escapes
(`\000` through `\377`). Octal escapes can encode the bytes of UTF-8 names.
These patterns work when attributes are read from the worktree, index or a commit tree.

## Linked worktrees (handle)

`Repository::worktrees` returns `RepositoryWorktrees`: list main and linked checkouts, add/remove/move/repair/lock linked worktrees with the same on-disk layout Git uses (`worktrees/` admin files, `gitdir` links). [`add_linked_worktree`](rustdoc:grit_lib::worktree::add_linked_worktree) runs the `post-checkout` hook from the new worktree after a successful checkout (null old id, start commit, flag `1`), matching Git; `--no-checkout` and orphan branches skip it. [`AddLinkedWorktreeOptions`](rustdoc:grit_lib::worktree::AddLinkedWorktreeOptions) uses [`WorktreeBranch`](rustdoc:grit_lib::worktree::WorktreeBranch) for branch modes (`Create`, `CreateOrReset`, `Existing`, `Detach`), optional lock-before-checkout, `--no-checkout`, and `--track`. [`move_linked_worktree`](rustdoc:grit_lib::worktree::move_linked_worktree) and [`repair_worktrees`](rustdoc:grit_lib::worktree::repair_worktrees) rewrite gitfile links atomically; repairs return [`WorktreeRepair`](rustdoc:grit_lib::worktree::WorktreeRepair) rows. [`prune_linked_worktrees`](rustdoc:grit_lib::worktree::prune_linked_worktrees) accepts [`PruneWorktreesOptions`](rustdoc:grit_lib::worktree::PruneWorktreesOptions) and returns [`PrunedWorktreeEntry`](rustdoc:grit_lib::worktree::PrunedWorktreeEntry) rows with a reason string.

Add, list and remove also work on a repository opened with an injected ref store or object store ([`RepositoryBuilder`](rustdoc:grit_lib::repo::RepositoryBuilder) or `with_ref_store`). Branches are created, read and rolled back in the injected store. The linked worktree's own `HEAD` lives in its admin directory (`worktrees/<id>/HEAD`), because the store holds only the main worktree's `HEAD`. Its branch tip is still resolved through the store, so `WorktreeEntry::head` matches what a files repository reports. Bareness comes from [`Repository::is_bare`](rustdoc:grit_lib::repo::Repository::is_bare), so a git directory kept outside the work tree is not mistaken for a bare repository.

To work in a linked worktree on the same injected stores, call [`Repository::open_linked_worktree`](rustdoc:grit_lib::repo::Repository::open_linked_worktree) on the main handle with the entry's `admin_dir` and path, or open the admin directory with `RepositoryBuilder::open(<git dir>/worktrees/<id>, Some(<linked path>))` and the same stores. Either handle reads and writes the worktree's own `HEAD` (`worktrees/<id>/HEAD`) and `HEAD` reflog (`worktrees/<id>/logs/HEAD`); every other ref and reflog stays in the injected store. A commit there moves the branch that file names, a detach or switch rewrites only that file, and the main worktree's branch, `HEAD` and `HEAD` reflog are left alone, as in a files repository. `with_ref_store` on a linked handle keeps the same split.

Refusals are typed. Linked worktree failures arrive as [`Error::Worktree`](rustdoc:grit_lib::error::Error::Worktree) carrying [`WorktreeError::Linked`](rustdoc:grit_lib::worktree::WorktreeError::Linked) with a [`LinkedWorktreeError`](rustdoc:grit_lib::worktree::LinkedWorktreeError): `NotRegistered`, `MainWorktree` (with the refused [`LinkedWorktreeOperation`](rustdoc:grit_lib::worktree::LinkedWorktreeOperation)), `Locked`, `AlreadyLocked`, `NotLocked`, `HasLocalChanges`, `HasSubmodules`, `PathRegistered`, `PathRegisteredMissing`, `PathExists`, `BareRepository` and so on. Branch problems during add use the same [`BranchError`](rustdoc:grit_lib::porcelain::branch::BranchError) variants as branch creation (`InvalidName`, `NotFound`, `AlreadyExists`). A branch that is already checked out in another worktree is reported as `Error::BranchCheckedOutInWorktree`, and a start point that is not a commit as `Error::UnexpectedObjectKind`.

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
