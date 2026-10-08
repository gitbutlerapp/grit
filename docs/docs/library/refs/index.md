# Refs

> Resolve HEAD, list branches and tags, update refs with reflog, and how grit-lib picks loose, packed, or reftable storage.

References name commits (and other objects). grit-lib exposes them through [`refs`](https://docs.rs/grit-lib/latest/grit_lib/refs/index.html) and [`reftable`](https://docs.rs/grit-lib/latest/grit_lib/reftable/index.html), with [`reflog`](https://docs.rs/grit-lib/latest/grit_lib/reflog/index.html) for update history. The [`references`](https://docs.rs/grit-lib/latest/grit_lib/references/index.html) module groups these for navigation in rustdoc.

## Resolving HEAD and symbolic refs

[`resolve_head`](https://docs.rs/grit-lib/latest/grit_lib/state/fn.resolve_head.html) reads `HEAD` and returns a [`HeadState`](https://docs.rs/grit-lib/latest/grit_lib/state/enum.HeadState.html): on a branch (symbolic ref plus commit, if any), detached at a commit, or invalid. To resolve any ref name to an object id, use [`resolve_ref`](https://docs.rs/grit-lib/latest/grit_lib/refs/fn.resolve_ref.html), which follows symbolic refs with cycle detection.

[`read_ref_file`](https://docs.rs/grit-lib/latest/grit_lib/refs/fn.read_ref_file.html) returns a [`Ref`](https://docs.rs/grit-lib/latest/grit_lib/refs/enum.Ref.html) (`Direct` or `Symbolic`) without resolving the whole chain.

## Listing branches and tags

[`list_refs`](https://docs.rs/grit-lib/latest/grit_lib/refs/fn.list_refs.html) takes a prefix such as `refs/heads/` or `refs/tags/` and returns sorted `(name, ObjectId)` pairs. Loose refs under `refs/` override stale lines in `packed-refs`, matching Git. [`list_refs_glob`](https://docs.rs/grit-lib/latest/grit_lib/refs/fn.list_refs_glob.html) applies pattern matching when you need DWIM-style filtering.

## Creating and updating refs with reflog

[`write_ref`](https://docs.rs/grit-lib/latest/grit_lib/refs/fn.write_ref.html) points a ref at a commit (or other object). [`write_symbolic_ref`](https://docs.rs/grit-lib/latest/grit_lib/refs/fn.write_symbolic_ref.html) updates symbolic refs such as `HEAD`.

Record history with [`append_reflog`](https://docs.rs/grit-lib/latest/grit_lib/refs/fn.append_reflog.html), then read it back with [`read_reflog`](https://docs.rs/grit-lib/latest/grit_lib/reflog/fn.read_reflog.html). Each [`ReflogEntry`](https://docs.rs/grit-lib/latest/grit_lib/reflog/struct.ReflogEntry.html) carries old and new ids, identity, and message. Batch updates can use [`update_refs`](https://docs.rs/grit-lib/latest/grit_lib/gc/fn.update_refs.html) when you need compare-and-swap semantics across many refs.

## Loose, packed, and reftable backends

By default, grit uses the **files** backend: one file per ref under `refs/`, plus an optional `packed-refs` file. [`list_refs`](https://docs.rs/grit-lib/latest/grit_lib/refs/fn.list_refs.html) and [`resolve_ref`](https://docs.rs/grit-lib/latest/grit_lib/refs/fn.resolve_ref.html) merge packed and loose sources so callers see a single namespace.

When `extensions.refStorage = reftable` is set in config, the same functions dispatch to the **reftable** backend ([`is_reftable_repo`](https://docs.rs/grit-lib/latest/grit_lib/reftable/fn.is_reftable_repo.html)). Reflog appends and ref listing go through reftable files instead of `logs/` and loose ref files. You do not choose the backend per call; discovery is automatic from the repository layout and config.

## Example

The program below resolves `HEAD`, lists branches and tags, updates `refs/heads/library-guide-demo`, and appends a reflog entry:

```rust
//! Resolve HEAD, list branches and tags, update a branch ref with reflog.
//!
//! Source for the library guide "Refs" page (included in the docs site).

use grit_lib::objects::ObjectId;
use grit_lib::reflog::read_reflog;
use grit_lib::refs::{self, Ref};
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
use std::path::{Path, PathBuf};

const DEMO_BRANCH: &str = "refs/heads/library-guide-demo";

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
    let git_dir = if root.join(".git").is_dir() {
        root.join(".git")
    } else {
        root.to_path_buf()
    };
    let work_tree = if root.join(".git").is_dir() {
        Some(root)
    } else {
        None
    };
    Repository::open(&git_dir, work_tree)
}

fn main() -> Result<(), grit_lib::error::Error> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| grit_lib::error::Error::Message("missing repository path".into()))?;
    let repo = open_repo(&root)?;
    let git_dir = &repo.git_dir;

    let head = resolve_head(git_dir)?;
    match &head {
        grit_lib::state::HeadState::Branch { refname, oid, .. } => {
            println!("head_symbolic={refname}");
            if let Some(oid) = oid {
                println!("head_oid={}", oid.to_hex());
            }
        }
        grit_lib::state::HeadState::Detached { oid } => {
            println!("head_detached={}", oid.to_hex());
        }
        grit_lib::state::HeadState::Invalid => println!("head_invalid=1"),
    }

    let head_file = refs::read_ref_file(&git_dir.join("HEAD"))?;
    if let Ref::Symbolic(target) = head_file {
        println!("head_file_symbolic={target}");
    }

    for (name, oid) in refs::list_refs(git_dir, "refs/heads/")? {
        println!("branch {name} {}", oid.to_hex());
    }
    for (name, oid) in refs::list_refs(git_dir, "refs/tags/")? {
        println!("tag {name} {}", oid.to_hex());
    }

    let storage = if grit_lib::reftable::is_reftable_repo(git_dir) {
        "reftable"
    } else {
        "files"
    };
    println!("ref_storage={storage}");

    let target = head
        .oid()
        .cloned()
        .ok_or_else(|| grit_lib::error::Error::Message("HEAD has no commit".into()))?;

    let old = refs::resolve_ref(git_dir, DEMO_BRANCH).ok();
    refs::write_ref(git_dir, DEMO_BRANCH, &target)?;

    let zero = ObjectId::zero();
    let old_oid = old.as_ref().unwrap_or(&zero);
    let identity = "Library Guide <guide@grit-scm.com> 1735689600 +0000";
    refs::append_reflog(
        git_dir,
        DEMO_BRANCH,
        old_oid,
        &target,
        identity,
        "library guide refs example",
        true,
    )?;

    println!("demo_ref={DEMO_BRANCH}");
    println!("demo_oid={}", target.to_hex());

    let entries = read_reflog(git_dir, DEMO_BRANCH)?;
    if let Some(last) = entries.last() {
        println!("demo_reflog_new={}", last.new_oid.to_hex());
        println!("demo_reflog_message={}", last.message);
    }

    Ok(())
}
```

Run against any repository with at least one commit:

```bash
cargo run --bin guide_refs /path/to/repo
git -C /path/to/repo rev-parse refs/heads/library-guide-demo
git -C /path/to/repo reflog show refs/heads/library-guide-demo
```
