---
title: Path component validation
summary: Reject hostile tree and worktree path components the same way Git fsck and verify_path do.
---

# Path component validation

Grit centralizes Git’s path-component rules in [`grit_lib::path_validate`](https://docs.rs/grit-lib/latest/grit_lib/path_validate/index.html). Use it before writing tree entries, populating the index, or checking out files so NTFS/HFS spellings and `.git` aliases cannot slip through.

## Protection options

[`ProtectOptions`](https://docs.rs/grit-lib/latest/grit_lib/path_validate/struct.ProtectOptions.html) mirrors `core.protectNTFS` (default **true**) and `core.protectHFS` (default **true** on macOS, **false** elsewhere). Load them from a merged [`ConfigSet`](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigSet.html):

```rust
use grit_lib::config::ConfigSet;
use grit_lib::path_validate::ProtectOptions;

let opts = ProtectOptions::from_config(&config);
```

Object fsck always enables both flags via [`ProtectOptions::fsck`](https://docs.rs/grit-lib/latest/grit_lib/path_validate/struct.ProtectOptions.html#method.fsck).

## Validating components and paths

[`validate_component`](https://docs.rs/grit-lib/latest/grit_lib/path_validate/fn.validate_component.html) checks a single tree/index segment (no `/` inside). Pass the Git mode so symlink-only rules apply to `.gitmodules`, `.gitattributes`, `.gitignore`, and `.mailmap`:

```rust
use grit_lib::path_validate::{validate_component, EntryMode, ProtectOptions};

let opts = ProtectOptions::fsck();
validate_component(b"src", EntryMode::new(0o100644), opts)?;
```

[`validate_path`](https://docs.rs/grit-lib/latest/grit_lib/path_validate/fn.validate_path.html) splits on `/` and validates each segment. [`audit_tree_entry_name`](https://docs.rs/grit-lib/latest/grit_lib/path_validate/fn.audit_tree_entry_name.html) returns a bitmask-style summary for fsck-style reporting when several rules fire on one name.

## Errors

[`PathComponentError`](https://docs.rs/grit-lib/latest/grit_lib/path_validate/enum.PathComponentError.html) is a typed, non-exhaustive enum (empty, `.`, `..`, slash/NUL, ASCII and platform `.git` aliases, NTFS backslash/drive/stream spellings, and symlinked dot-files). It carries Grit-owned messages for debugging; the CLI maps failures to [`Error::InvalidPath`](https://docs.rs/grit-lib/latest/grit_lib/error/enum.Error.html) where appropriate.

## Related

- Tree object fsck uses the same rules via `grit-lib` maintenance APIs.
- Step 2 of the path hardening plan will enforce these checks at index population and checkout.
