# Design: byte-correct paths (non-UTF-8 path support)

Status: draft / RFC
Author: (drafted with Claude Code)
Date: 2026-06-22
Origin: Discord discussion — Caleb Owens & Byron — on `grit-lib` forcing OS
paths into UTF-8 `String`/`&str`.

## Problem

Git treats pathnames in trees, the index, and the wire protocol as **arbitrary
byte strings** (only `NUL` and, by component, `/` are special). On Unix the
kernel agrees: `OsStr` is bytes, not UTF-8. `grit` decodes many of these byte
paths into Rust `String`/`&str` via `String::from_utf8_lossy`, which **replaces
invalid bytes with U+FFFD**. That is lossy and irreversible:

- A tree entry whose name is `caf\xe9` (Latin-1) round-trips to `caf<U+FFFD>`,
  so a checkout, diff, or status against it produces the wrong filename — and a
  re-commit would change the tree OID.
- Any operation keyed on the path string (pathspec match, rename detection,
  index lookup) silently mismatches the real on-disk name.

This is a correctness bug for repos containing non-UTF-8 paths, not a
theoretical nicety. Real-world repos have them (legacy encodings, deliberate
test fixtures, `git`'s own `t/` suite).

### Byron's constraint (important)

The fix is **not** "replace `String` with bare `BString`." `gix` did that and
regrets it: a bare `BString` is indistinguishable from "any byte blob," so it
becomes an *implicit* `RepoPath` whose meaning depends on call-site context.
We want a **dedicated newtype** that encodes the invariant ("this is a
repo-relative, `/`-separated path in Git's byte encoding") and makes the
filesystem boundary explicit and platform-aware.

## Current state (grounded)

Good news: the **storage layer already keeps bytes.** The lossiness is injected
at a small number of *boundaries*, not baked into the core data structures.

| Layer | Type today | Lossy? |
|---|---|---|
| Tree entry name — `objects.rs:366` `TreeEntry.name` | `Vec<u8>` | ✅ byte-correct |
| Tree parse — `objects.rs:445` | `data[..nul].to_vec()` | ✅ byte-correct |
| Index entry path — `index.rs:87` `IndexEntry.path` | `Vec<u8>` | ✅ byte-correct |
| Index parse (v2/v3/v4) — `index.rs:2133,2140` | `to_vec()` | ✅ byte-correct |
| **DiffEntry** — `diff.rs:1373,1375` `old_path`/`new_path` | `Option<String>` | ❌ `from_utf8_lossy` at `diff.rs:1550,1595,1638,1678` |
| **Pathspec** — `pathspec.rs:604,877,1328` | `&str` / `&[String]` | ❌ operates on lossy `&str` |
| **Working-tree join** — `porcelain/checkout.rs:125`, `diff.rs:2070` | `work_tree.join(rel: &str)` | ❌ joins a lossy `&str` |
| Pathspec source parse — `pathspec.rs:47` | `from_utf8_lossy` | ❌ |
| Tree → diff name — `diff.rs:1550`; merge — `merge_trees.rs:673` | `from_utf8_lossy` | ❌ |

Existing platform-aware byte conversions already exist and are the model to
generalize:

- `crlf.rs:417` `path_to_index_bytes(&Path)` — uses
  `std::os::unix::ffi::OsStrExt::as_bytes()`, with a `to_string_lossy` fallback
  on non-Unix.
- `ident_resolve.rs:135` — `OsStrExt::as_bytes()` on env vars.

There is **no** `RepoPath` newtype today, and `bstr` is **not** a workspace
dependency. `grit-utils` is a benchmark binary (only `main.rs`, no `lib.rs`), so
it is *not* the home for shared types. Existing path modules live in `grit-lib`:
`git_path.rs`, `path.rs`, `pathspec.rs`, `quote_path.rs`, `transport_path.rs`,
`tree_path_follow.rs`.

There are ~291 `to_string_lossy`/`from_utf8_lossy`/`to_str()` calls in
`grit-lib/src` and ~54 `String`-typed path fields. Most are at boundaries; the
core stays bytes.

## Proposed design

### 1. The newtype pair

Add a new module `grit-lib/src/repo_path.rs` exporting an owned/borrowed pair,
modeled on `PathBuf`/`Path` and `String`/`str`:

```rust
/// An owned, repo-relative, `/`-separated path in Git's byte encoding.
/// Invariants: no `NUL`; `/`-separated; not normalized for `.`/`..` here.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPathBuf(BString);

/// Borrowed counterpart. `&RepoPath` is to `RepoPathBuf` as `&Path` is to `PathBuf`.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(BStr); // unsized, used behind &
```

Core API (intentionally small; grow as needed):

```rust
impl RepoPath {
    pub fn from_bytes(b: &[u8]) -> &RepoPath;     // the canonical constructor
    pub fn as_bytes(&self) -> &[u8];
    pub fn to_str(&self) -> Option<&str>;         // Some iff valid UTF-8
    pub fn display(&self) -> impl Display;        // lossy, for humans ONLY
    pub fn file_name(&self) -> Option<&RepoPath>;
    pub fn parent(&self) -> Option<&RepoPath>;
    pub fn components(&self) -> impl Iterator<Item = &RepoPath>;
    pub fn join(&self, other: &RepoPath) -> RepoPathBuf;

    /// The platform boundary: build a real filesystem path under `root`.
    pub fn to_fs_path(&self, root: &Path) -> PathBuf;   // unix: bytes->OsStr; else lossy+warn
}

impl RepoPathBuf {
    pub fn from_bytes(v: Vec<u8>) -> Self;
    pub fn from_fs_relative(p: &Path) -> Self;          // OsStr bytes -> RepoPath, normalize `\`->`/` on win
}
```

Conversions are **explicit**: there is no `From<String>`/`Deref<Target=str>`,
because that is exactly the implicit-coercion trap Byron warned about. Display
to a human goes through `.display()` (lossy and clearly named); programmatic
UTF-8 access goes through `.to_str() -> Option`.

### 2. Where the bytes/OS boundary sits

One function — `RepoPath::to_fs_path` / `RepoPathBuf::from_fs_relative` — owns
the conversion between Git's byte path and the OS path:

- **Unix**: `OsStrExt::{as_bytes, from_bytes}` — zero loss.
- **Windows**: Git paths are UTF-8 (or GBK etc.) and `/`-separated; convert via
  the existing lossy fallback and translate separators. Windows already can't
  represent arbitrary bytes in filenames, so lossiness there is inherent to the
  OS, not to us. (Matches `crlf.rs` strategy.)

Every `work_tree.join(rel_str)` site (`checkout.rs:125`, `diff.rs:2070`, …)
becomes `repo_path.to_fs_path(work_tree)`.

### 3. Pathspec matching on bytes

`pathspec.rs` is the one place with real *logic* on paths (globbing, prefix,
icase). Migrate its core matchers to operate on `&[u8]`/`&RepoPath`. Git's own
pathspec matching is byte-oriented, so this also fixes latent glob bugs on
non-ASCII. `&str`-typed public entry points can keep thin shims during
migration.

### 4. `bstr` dependency

Add `bstr` to `[workspace.dependencies]` and depend on it from `grit-lib`. We
use it as the *inner representation and the byte-string utility library* (Unicode-aware
`to_str_lossy`, `Display`, search), but never expose bare `BStr`/`BString` in
public path signatures — only `RepoPath`/`RepoPathBuf`.

## Migration plan (phased, each phase ships independently)

The storage layer is already bytes, so we migrate **outward from the boundaries**.

- **Phase 0 — foundation (no behavior change).**
  Add `bstr`; implement `repo_path.rs` with the newtype, conversions, and a unit
  test using a literal non-UTF-8 byte path (`b"caf\xe9"`). Wire `to_fs_path`
  through the existing `crlf.rs` platform logic so there's one boundary impl.

- **Phase 1 — DiffEntry vertical slice (the screenshot).**
  `DiffEntry.old_path`/`new_path: Option<RepoPathBuf>`. Replace the
  `from_utf8_lossy` at `diff.rs:1550/1595/1638/1678` with byte construction.
  Update diff *formatters* to call `.display()` (or `quote_path` for `-z`/quoting).
  Add a t-test fixture: diff/status over a repo containing a non-UTF-8 filename,
  asserting the bytes survive a `commit`→`diff`→`checkout` round-trip.

- **Phase 2 — working-tree I/O.**
  Route checkout/apply/status writes through `to_fs_path`. This is where the
  *user-visible* corruption (wrong files on disk) actually gets fixed.

- **Phase 3 — pathspec on bytes.**
  Migrate matcher cores to `&[u8]`; keep `&str` shims; delete shims once callers
  pass `RepoPath`.

- **Phase 4 — sweep the long tail.**
  Remaining `String` path fields (`apply.rs`, `show.rs`, `am.rs`, the CLI
  command structs that mirror `DiffEntry`). Mostly mechanical once the newtype
  exists.

Each phase is independently mergeable and verified against the `data/tests` t-suite.

## Testing strategy

1. **Unit**: round-trip `b"caf\xe9/\xff.txt"` through `RepoPathBuf` → bytes,
   `from_fs_relative`/`to_fs_path` on Unix, and assert `to_str()` is `None`.
2. **Integration t-test** (new `data/tests/.../*.toml`): create a file with a
   non-UTF-8 name, `add`/`commit`, then `diff`, `status`, `ls-files`,
   `checkout` and assert byte-exact output and that the working-tree file
   matches the original bytes. Compare against real `git` output.
3. **Regression**: full t-suite must stay green at each phase (boundaries change
   type, not behavior, for UTF-8 paths).

## Open questions

1. **Home**: `grit-lib/src/repo_path.rs` (proposed) vs. a new `grit-path` crate
   shared by `grit-lib`/`grit-cli`/`grit-protocol`. Lean module-first; promote
   to a crate only if `grit-cli`/`grit-protocol` need it directly.
2. **`bstr` vs hand-rolled**: `bstr` buys Unicode-aware lossy display + search
   for ~one dependency. Alternative is `Vec<u8>` + `os_str_bytes`. Recommend
   `bstr` (matches the ecosystem `gix` is converging toward).
3. **Quoting**: how `RepoPath::display` interacts with the existing
   `quote_path.rs` (`core.quotePath`, `-z`) — display is for logs; user-facing
   command output must continue to route through `quote_path`.
4. **Wire/protocol paths**: `transport_path.rs` and protocol pathnames are also
   bytes; in scope for a later phase but not Phases 0–2.

## Recommendation

Adopt the `RepoPath`/`RepoPathBuf` newtype in `grit-lib` backed by `bstr`, with
the single platform boundary in `to_fs_path`/`from_fs_relative`. Do **not**
expose bare `BString`. Execute Phase 0 + Phase 1 first (foundation + the
`DiffEntry` slice from the discussion) as a reviewable proof point before
committing to the full sweep.
