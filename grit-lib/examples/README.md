# grit-lib examples

Small, self-contained programs that show how to use **grit-lib** (the Git-compatible engine behind the `grit` binary). Each example creates a temporary repository with [`tempfile`](https://docs.rs/tempfile), so nothing touches your working tree.

## Running an example

From the workspace root:

```bash
cargo run -p grit-lib --example <name>
```

Example:

```bash
cargo run -p grit-lib --example odb_blob
```

The crate name is `grit-lib`; sources live in this directory as `examples/<name>.rs`.

---

## `repo_init_and_open`

Creates a non-bare repo with `init_repository` (same idea as `git init`), then shows two ways to get a `Repository`: explicit `Repository::open` with a known git dir, and `Repository::discover` after changing into a subdirectory—matching how tools find `.git` when the current directory is not the repo root.

**Interesting bits:** `explicit_git_dir`, work tree vs bare, discovery from a nested path.

---

## `odb_blob`

Writes a **blob** with `Odb::write` and reads it back with `Odb::read`. This is the smallest object-store round trip: zlib-compressed loose objects under `.git/objects/`.

**Interesting bits:** `ObjectKind::Blob`, `Object` payload (header already stripped after read).

---

## `index_add`

Builds an empty `Index`, inserts a synthetic `IndexEntry` pointing at a staged blob OID, and persists with `Repository::write_index`. Reloads with `Repository::load_index` to show the index round trip.

**Interesting bits:** index entry metadata (mode, flags, path bytes), staging without touching the working tree—useful for tests and plumbing tools.

---

## `commit_tree`

Full “write-tree + commit + ref” path: stage files → `write_tree_from_index` → `serialize_commit` / `CommitData` → `Odb::write` → `refs::write_ref` to move `refs/heads/main`.

**Interesting bits:** how a commit object is just bytes; updating a branch ref after the fact.

---

## `rev_list`

Builds a commit, points `HEAD` at it, then calls `rev_list` with `RevListOptions` (here, `OutputMode::OidOnly`) and a positive spec `"HEAD"`. This is the same revision-walk machinery used for `git rev-list`-style operations.

**Interesting bits:** positive vs negative specs (empty here), `RevListResult::commits`.

---

## `pack_index`

Lists **pack indexes** under `objects/pack/` via `pack::read_local_pack_indexes`. A freshly initialized repo has no packs, so the example explains that; if `.idx` files exist, it also demonstrates `pack::read_pack_index` on a file.

**Interesting bits:** pack vs loose objects; `.idx` maps OIDs to offsets in the `.pack` file.

---

## `rev_parse`

Creates a commit, updates `refs/heads/main`, then resolves `"HEAD"` and a full 40-character hex string with `rev_parse::resolve_revision`—the same resolution layer used for symbolic refs, abbreviations, and object peeling in more complex commands.

**Interesting bits:** `HEAD` as symref vs raw object id.

---

## `merge_base`

Constructs two branch tips that share an ancestor (`main` stays on the root commit, `feature` advances one commit), then calls `merge_base::merge_bases_first_vs_rest` to compute merge bases (the shared commit).

**Interesting bits:** merge base as “best” common ancestor; typical input for merges and rebases.

---

## `ignore_match`

Writes a `.gitignore` in the work tree, builds `IgnoreMatcher::from_repository`, and queries paths with `IgnoreMatcher::check_path`. Shows ignored vs not ignored and optional `IgnoreMatch` metadata for verbose-style output.

**Interesting bits:** precedence of exclude sources; `check-ignore`-style evaluation without invoking the binary.

---

## `walk_tree`

Builds a small tree with nested paths, resolves `HEAD`, reads the commit’s **tree** OID, then recursively walks with `objects::parse_tree`. Subtrees use `MODE_TREE`; blobs print as `path -> oid`.

**Interesting bits:** tree objects are flat lists of `(mode, name, oid)`; directories are separate tree objects referenced by OID.

---

## `cherry_pick`

Implements a minimal **cherry-pick**: `main` and `topic` diverge such that `topic` adds a file on top of `main`’s tree. It then runs `merge_trees::merge_trees_three_way` with **base** = parent of the picked commit, **ours** = `HEAD`, **theirs** = picked commit’s tree—the same three-way setup as Git’s sequencer for a pick. Writes the merged index to a tree, creates a new commit with `commit_trailers::finalize_cherry_pick_message` (adds the `(cherry picked from commit …)` line when configured like `-x`).

**Interesting bits:** `MergeFavor` and `WhitespaceMergeOptions` for content merges; `TreeMergeOutput::conflict_content` when paths conflict (this example expects a clean merge).

---

## See also

- API reference: `cargo doc -p grit-lib --open`
- The `grit` CLI (`grit-cli`) is built on the same library; see `TESTING.md` for how the workspace is tested.
