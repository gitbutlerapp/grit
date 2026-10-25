---
title: Worktree
summary: Linked worktrees, ignore rules, attributes, and pathspec helpers under the worktree domain.
---

Working-tree behavior lives under [`worktree`](rustdoc:grit_lib::worktree): ignore/attributes (`worktree::ignore`, `worktree::attributes`), pathspecs (`worktree::pathspec`), and checkout helpers. Porcelain commands (`status`, `add`, `checkout`, …) compose these modules with the index and ODB.

## Matching paths against pathspecs

To filter a path list by user-typed pathspecs, resolve each one with [`Repository::resolve_worktree_pathspec`](rustdoc:grit_lib::repo::Repository::resolve_worktree_pathspec), then build a [`PathspecMatcher`](rustdoc:grit_lib::pathspec::PathspecMatcher) once with [`Repository::pathspec_matcher`](rustdoc:grit_lib::repo::Repository::pathspec_matcher) and ask it about each path. It applies Git's list rules (any positive spec, no `:(exclude)` spec) and every magic word, including `:(attr:...)`. Attributes come from the repository's own `.gitattributes` stack (or `info/attributes` in a bare repository). `[attr]` macros are expanded, and `core.ignorecase` makes attribute patterns case-insensitive, as in Git. Attributes are only loaded when a spec asks for them, once per matcher rather than once per path. Building the matcher returns a `Result`: a malformed `attr:` clause, such as an empty `:(attr:)` or two `attr:` clauses in one spec, fails with [`Error::InvalidAttrPathspec`](rustdoc:grit_lib::error::Error::InvalidAttrPathspec) carrying an [`AttrPathspecError`](rustdoc:grit_lib::error::AttrPathspecError). `matches_change` also checks the old path of a rename. [`Repository::paths_match_pathspecs`](rustdoc:grit_lib::repo::Repository::paths_match_pathspecs) is the one-path shorthand. The free functions in `worktree::pathspec` (`matches_pathspec_list` and friends) have no attributes, so they never match a positive `:(attr:...)` spec. Use [`pathspec_has_attr_magic`](rustdoc:grit_lib::pathspec::pathspec_has_attr_magic) to detect those specs, or pass your own rules to [`matches_pathspec_list_for_object`](rustdoc:grit_lib::pathspec::matches_pathspec_list_for_object).

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
