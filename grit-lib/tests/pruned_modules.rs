//! Guard against reintroducing grit-git-only modules removed in ROADMAP item 5.
//!
//! See `docs/v1-scope.md` (Removed from grit-lib).

use std::fs;
use std::path::PathBuf;

/// Source files that must not exist under `grit-lib/src`.
const REMOVED_SOURCE_FILES: &[&str] = &[
    "am.rs",
    "mailinfo.rs",
    "difftool.rs",
    "mergetool_vimdiff.rs",
    "fast_import.rs",
    "fast_export.rs",
    "branch_ref_format.rs",
    "git_column.rs",
    "merge_tree_trivial.rs",
    "simple_ipc.rs",
    "tab_expand.rs",
    "unix_process.rs",
    "pack_geometry.rs",
    "instaweb/mod.rs",
    "porcelain/format_patch.rs",
];

/// Module names that must not appear as `mod` / `pub mod` declarations.
const REMOVED_MODULE_NAMES: &[&str] = &[
    "am",
    "mailinfo",
    "format_patch",
    "instaweb",
    "difftool",
    "mergetool_vimdiff",
    "fast_import",
    "fast_export",
    "simple_ipc",
    "merge_tree_trivial",
    "git_column",
    "tab_expand",
    "branch_ref_format",
    "unix_process",
    "pack_geometry",
];

#[test]
fn pruned_module_files_stay_absent() {
    let src = manifest_src_dir();
    for rel in REMOVED_SOURCE_FILES {
        let path = src.join(rel);
        assert!(
            !path.exists(),
            "removed module file must not exist: {}",
            path.display()
        );
    }
}

#[test]
fn lib_rs_and_porcelain_mod_do_not_declare_pruned_modules() {
    let src = manifest_src_dir();
    let lib_rs = read_utf8(&src.join("lib.rs"));
    assert_no_removed_mod_declarations(&lib_rs, "src/lib.rs");
    let porcelain = read_utf8(&src.join("porcelain/mod.rs"));
    assert_no_removed_mod_declarations(&porcelain, "src/porcelain/mod.rs");
}

fn manifest_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn read_utf8(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn assert_no_removed_mod_declarations(source: &str, label: &str) {
    for name in REMOVED_MODULE_NAMES {
        for line in source.lines() {
            let trimmed = line.split("//").next().unwrap_or(line).trim();
            if declares_mod(trimmed, name) {
                panic!("{label} declares removed module `{name}`: {trimmed}");
            }
        }
    }
}

fn declares_mod(line: &str, name: &str) -> bool {
    let patterns = [
        format!("mod {name};"),
        format!("mod {name} {{"),
        format!("pub mod {name};"),
        format!("pub mod {name} {{"),
        format!("pub(crate) mod {name};"),
        format!("pub(crate) mod {name} {{"),
    ];
    patterns.iter().any(|p| line.starts_with(p))
}
