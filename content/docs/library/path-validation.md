---
title: Path component validation
summary: Reject hostile tree and worktree path components the same way Git fsck and verify_path do.
---

# Path component validation

Grit centralizes Git’s path-component rules in [`grit_lib::path_validate`](rustdoc:grit_lib::path_validate). Use it before writing tree entries, populating the index, or checking out files so NTFS/HFS spellings and `.git` aliases cannot slip through.

## Protection options

[`ProtectOptions`](rustdoc:grit_lib::path_validate::ProtectOptions) mirrors `core.protectNTFS` (default **true**) and `core.protectHFS` (default **true** on macOS, **false** elsewhere). Load them from a merged [`ConfigSet`](rustdoc:grit_lib::config::ConfigSet):

```rust
use grit_lib::config::ConfigSet;
use grit_lib::path_validate::ProtectOptions;

let opts = ProtectOptions::from_config(&config);
```

Object fsck always enables both flags via [`ProtectOptions::fsck`](rustdoc:grit_lib::path_validate::ProtectOptions::fsck).

## Validating components and paths

[`validate_component`](rustdoc:grit_lib::path_validate::validate_component) checks a single tree/index segment (no `/` inside). Pass the Git mode and a [`PathValidationKind`](rustdoc:grit_lib::path_validate::PathValidationKind). Worktree population and checkout use **`Worktree`**. On Windows with `core.protectNTFS` (default), each segment is checked with Git’s `is_valid_win32_path` rules: forbidden characters (`:*?"<>|` and controls), trailing spaces or periods (except `.` and `..`), DOS device names (`CON`, `AUX`, `COM1`, …), plus NTFS drive prefixes, literal backslashes in the name, and backslash-`.git` spellings. Symlink names reject **`.gitmodules`** on worktree; object fsck uses **`Fsck`** (all four symlink dot-files):

```rust
use grit_lib::path_validate::{validate_component, EntryMode, PathValidationKind, ProtectOptions};

let opts = ProtectOptions::fsck();
validate_component(b"src", EntryMode::new(0o100644), opts, PathValidationKind::Worktree)?;
```

Fsck checks all symlink dot-files (`.gitmodules`, `.gitattributes`, `.gitignore`, `.mailmap`):

```rust
use grit_lib::path_validate::{validate_component, EntryMode, PathValidationKind, ProtectOptions};

let opts = ProtectOptions::fsck();
validate_component(
    b".gitignore",
    EntryMode::new(0o120000),
    opts,
    PathValidationKind::Fsck,
)?;
```

## `.git` aliases

With `core.protectNTFS` on, every platform rejects the NTFS spellings of `.git` for both kinds, because a repository checked out on Linux may later be used from Windows. These are `.git` and its 8.3 short name `git~1` in any case, each optionally followed by spaces and periods (which NTFS strips) or by a `:` stream suffix. Examples are `.git.`, `GIT~1.`, `git~1  ` and `.git::$INDEX_ALLOCATION`. A `\`-separated segment that spells `.git`, such as `.git\config`, is rejected too. System git refuses the same names on Linux, so a hostile tree cannot plant a second `.git` directory through either implementation.

[`validate_path`](rustdoc:grit_lib::path_validate::validate_path) splits on `/` and validates each segment. [`audit_tree_entry_name`](rustdoc:grit_lib::path_validate::audit_tree_entry_name) returns a bitmask-style summary for fsck-style reporting when several rules fire on one name.

## Errors

[`PathComponentError`](rustdoc:grit_lib::path_validate::PathComponentError) is a typed, non-exhaustive enum (empty, `.`, `..`, slash/backslash/NUL, ASCII and platform `.git` aliases, NTFS backslash-`.git`/drive/stream spellings, NTFS illegal characters, trailing space/period, reserved DOS device names, and symlinked dot-files). Checkout and index population return [`WorktreeError::InvalidPathComponent`](rustdoc:grit_lib::worktree::WorktreeError::InvalidPathComponent) with the full path and reason. User-facing `add`/`mv` path arguments use the same typed error; legacy [`Error::InvalidPath`](rustdoc:grit_lib::error::Error) remains for empty or escaping paths.

## Related

- Ref names under `refs/` use [`RefName::parse`](rustdoc:grit_lib::refs::check_ref_format::RefName::parse) with [`RefNamePolicy::storable`](rustdoc:grit_lib::refs::check_ref_format::RefNamePolicy::storable) or [`safe_for_fs`](rustdoc:grit_lib::refs::check_ref_format::RefNamePolicy::safe_for_fs) instead of the old free validators. See [Refs](refs.md#ref-names-refname--refnamepolicy).
- Tree object fsck uses the same rules via `grit-lib` maintenance APIs.
- Index population, checkout, restore, and `grit add` staging run these validators internally with [`ProtectOptions::from_config`](rustdoc:grit_lib::path_validate::ProtectOptions::from_config). Code that writes its own tree or index entries calls [`validate_path`](rustdoc:grit_lib::path_validate::validate_path) or [`validate_component`](rustdoc:grit_lib::path_validate::validate_component) directly.
