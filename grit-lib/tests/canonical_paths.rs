//! Compile-time checks that canonical domain paths resolve for workspace consumers.

use grit_lib::{
    config::ConfigSet,
    diff::DiffEntry,
    index::Index,
    objects::odb::store::MemoryStore,
    objects::{bitmap::BitmapIndex, hash::ObjectHasher, odb::Odb},
    parallel::ThreadPolicy,
    refs::{check_ref_format::RefNameOptions, reflog::ReflogEntry},
    remote::credentials::Credential,
    repo::Repository,
    revwalk::rev_list::RevListOptions,
    stash::StashEntry,
    worktree::ignore::IgnoreMatcher,
};

#[test]
fn canonical_paths_cover_external_uses() {
    let _ = std::any::type_name::<Repository>();
    let _ = std::any::type_name::<Odb>();
    let _ = std::any::type_name::<Index>();
    let _ = std::any::type_name::<DiffEntry>();
    let _ = std::any::type_name::<ConfigSet>();
    let _ = std::any::type_name::<BitmapIndex>();
    let _ = std::any::type_name::<ObjectHasher>();
    let _ = std::any::type_name::<ThreadPolicy>();
    let _ = std::any::type_name::<ReflogEntry>();
    let _ = std::any::type_name::<RefNameOptions>();
    let _ = std::any::type_name::<MemoryStore>();
    let _ = std::any::type_name::<Credential>();
    let _ = std::any::type_name::<RevListOptions>();
    let _ = std::any::type_name::<StashEntry>();
    let _ = std::any::type_name::<IgnoreMatcher>();

    let map = include_str!("../canonical-modules.toml");
    assert!(
        map.contains("[map]"),
        "canonical-modules.toml must define [map]"
    );
}

#[test]
fn canonical_layout_script_covers_workspace_imports() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let status = std::process::Command::new("python3")
        .arg("scripts/check_canonical_layout.py")
        .current_dir(workspace)
        .status()
        .expect("run check_canonical_layout.py");
    assert!(status.success(), "canonical layout map is incomplete");
}

/// Maximum root-level `pub mod` declarations in `grit-lib/src/lib.rs` (excluding `prelude`).
///
/// Raise only when adding a new top-level domain module. The canonical layout on
/// `origin/main` declares **36** embedder-facing roots; **`test_support`** is
/// feature-gated and excluded via [`ROOT_PUB_MOD_CFG_EXEMPT`].
const ROOT_PUB_MOD_BUDGET: usize = 36;

/// Root modules gated behind `test` / `test-support` / other non-default features
/// do not count toward the embedder-surface budget.
const ROOT_PUB_MOD_CFG_EXEMPT: &[&str] = &["test_support"];

#[test]
fn root_public_module_count_within_budget() {
    let lib_rs = include_str!("../src/lib.rs");
    let mut prev_cfg_gated = false;
    let count = lib_rs
        .lines()
        .filter(|line| {
            let t = line.trim();
            if t.starts_with("#[cfg") {
                prev_cfg_gated = t.contains("test-support") || t.contains("test,");
                return false;
            }
            if t.starts_with("pub mod ") && !t.starts_with("pub mod prelude") {
                let name = t.strip_prefix("pub mod ").and_then(|rest| rest.split_whitespace().next());
                let exempt = name.is_some_and(|n| ROOT_PUB_MOD_CFG_EXEMPT.contains(&n));
                let count_this = !prev_cfg_gated && !exempt;
                prev_cfg_gated = false;
                return count_this;
            }
            prev_cfg_gated = false;
            false
        })
        .count();
    assert!(
        count <= ROOT_PUB_MOD_BUDGET,
        "expected at most {ROOT_PUB_MOD_BUDGET} root pub mod declarations, found {count}"
    );
}

#[test]
fn no_orphan_gitlinks_without_gitmodules() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let status = std::process::Command::new("python3")
        .arg("scripts/check_gitlink_gitmodules.py")
        .current_dir(workspace)
        .status()
        .expect("run check_gitlink_gitmodules.py");
    assert!(
        status.success(),
        "every mode-160000 index entry must appear in .gitmodules"
    );
}

#[test]
fn no_noncanonical_grit_lib_imports() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let status = std::process::Command::new("python3")
        .arg("scripts/check_noncanonical_grit_lib_imports.py")
        .current_dir(workspace)
        .status()
        .expect("run check_noncanonical_grit_lib_imports.py");
    assert!(
        status.success(),
        "workspace must use canonical grit_lib domain paths only"
    );
}
