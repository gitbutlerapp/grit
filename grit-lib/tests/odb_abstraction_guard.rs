//! Workspace guard: pack-index and loose-enumeration helpers must stay inside ODB/pack/MIDX modules.
//!
//! Step 10 (`factory/route-write-odb-670`) removes the temporary allowlist entries below.

use std::path::{Path, PathBuf};

fn forbidden_tokens() -> [&'static str; 4] {
    [
        "read_local_pack_indexes(",
        "read_local_pack_indexes_cached(",
        "for_each_loose_object_id(",
        "enumerate_loose_objects(",
    ]
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("grit-lib has a parent directory")
        .to_path_buf()
}

fn is_excluded(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    if path_str.contains("/target/") || path_str.contains("\\target\\") {
        return true;
    }
    if path.extension().is_none_or(|e| e != "rs") {
        return true;
    }
    if path
        .file_name()
        .is_some_and(|n| n == "odb_abstraction_guard.rs")
    {
        return true;
    }

    // ODB, pack, and MIDX implementation trees.
    if path_str.contains("/grit-lib/src/odb/")
        || path_str.contains("\\grit-lib\\src\\odb\\")
        || path_str.contains("/grit-lib/src/pack")
        || path_str.contains("\\grit-lib\\src\\pack")
        || path_str.contains("/grit-lib/src/midx.rs")
        || path_str.contains("\\grit-lib\\src\\midx.rs")
    {
        return true;
    }

    // Temporary allowlist — removed when step 10 routes write-path callers through traits.
    const S10_ALLOWLIST: &[&str] = &[
        "/grit-lib/src/porcelain/add.rs",
        "\\grit-lib\\src\\porcelain\\add.rs",
        "/grit-lib/src/porcelain/stage_tracked.rs",
        "\\grit-lib\\src\\porcelain\\stage_tracked.rs",
        "/grit-lib/src/unpack_objects.rs",
        "\\grit-lib\\src\\unpack_objects.rs",
        "/grit-lib/src/write_tree.rs",
        "\\grit-lib\\src\\write_tree.rs",
        "/grit-lib/src/gitmodules.rs",
        "\\grit-lib\\src\\gitmodules.rs",
        "/grit-lib/src/index_pack.rs",
        "\\grit-lib\\src\\index_pack.rs",
    ];
    if S10_ALLOWLIST.iter().any(|p| path_str.contains(p)) {
        return true;
    }

    // Direct pack-index inspection tools and conformance tests.
    path_str.contains("/grit-lib/examples/pack_index.rs")
        || path_str.contains("\\grit-lib\\examples\\pack_index.rs")
        || path_str.contains("/grit-lib/tests/pack_index_formats.rs")
        || path_str.contains("\\grit-lib\\tests\\pack_index_formats.rs")
}

fn scan_rs_files(dir: &Path, violations: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            scan_rs_files(&path, violations);
        } else if path.extension().is_some_and(|e| e == "rs") && !is_excluded(&path) {
            let text = std::fs::read_to_string(&path).expect("read rust source");
            for (line_no, line) in text.lines().enumerate() {
                for token in forbidden_tokens() {
                    if line.contains(token) {
                        violations.push(format!(
                            "{}:{}: forbidden token `{token}`",
                            path.display(),
                            line_no + 1
                        ));
                    }
                }
            }
        }
    }
}

#[test]
fn no_direct_pack_or_loose_enumeration_outside_odb_modules() {
    let mut violations = Vec::new();
    scan_rs_files(&workspace_root(), &mut violations);
    assert!(
        violations.is_empty(),
        "direct pack-index / loose-path enumeration must go through Odb / ObjectStore:\n{}",
        violations.join("\n")
    );
}
