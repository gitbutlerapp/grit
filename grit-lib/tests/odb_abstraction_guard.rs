//! Workspace guard: pack-index and loose-enumeration helpers must stay inside ODB/pack/MIDX modules,
//! and library code must not probe loose paths or read pack indexes directly outside the store layer.

use std::path::{Path, PathBuf};

fn enumeration_forbidden_tokens() -> [&'static str; 4] {
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
        .is_some_and(|n| n == "odb_abstraction_guard.rs" || n == "hash_abstraction_guard.rs")
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
        || path_str.ends_with("/grit-lib/src/objects.rs")
        || path_str.ends_with("\\grit-lib\\src\\objects.rs")
        || path_str.contains("/grit-lib/src/pack_store")
        || path_str.contains("\\grit-lib\\src\\pack_store")
        || path_str.contains("/grit-lib/src/prune_packed.rs")
        || path_str.contains("\\grit-lib\\src\\prune_packed.rs")
    {
        return true;
    }

    // Pack-ingest exceptions: read a just-written index before it is in the ODB cache.
    const PACK_INDEX_READ_ALLOWLIST: &[&str] = &[
        "/grit-lib/src/index_pack.rs",
        "\\grit-lib\\src\\index_pack.rs",
        "/grit-lib/src/pack_rev.rs",
        "\\grit-lib\\src\\pack_rev.rs",
    ];
    if PACK_INDEX_READ_ALLOWLIST
        .iter()
        .any(|p| path_str.contains(p))
    {
        return true;
    }

    // Direct pack-index inspection tools and conformance tests.
    if path_str.contains("/grit-lib/examples/pack_index.rs")
        || path_str.contains("\\grit-lib\\examples\\pack_index.rs")
        || path_str.contains("/grit-lib/tests/pack_index_formats.rs")
        || path_str.contains("\\grit-lib\\tests\\pack_index_formats.rs")
    {
        return true;
    }

    // Criterion / fixture builders (not production library paths).
    path_str.contains("/grit-lib/benches/") || path_str.contains("\\grit-lib\\benches\\")
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
                let trimmed = line.trim();
                if trimmed.starts_with("//") || trimmed.starts_with('*') {
                    continue;
                }
                for token in enumeration_forbidden_tokens() {
                    if line.contains(token) {
                        violations.push(format!(
                            "{}:{}: forbidden token `{token}`",
                            path.display(),
                            line_no + 1
                        ));
                    }
                }
                if line.contains(".object_path(") || line.contains("loose_path_in(") {
                    violations.push(format!(
                        "{}:{}: direct loose path probe",
                        path.display(),
                        line_no + 1
                    ));
                }
                if line.contains("read_pack_index(") {
                    violations.push(format!(
                        "{}:{}: direct read_pack_index (use PackedObjects or allowlist)",
                        path.display(),
                        line_no + 1
                    ));
                }
            }
        }
    }
}

fn production_scan_roots() -> Vec<PathBuf> {
    let root = workspace_root();
    [
        "grit-lib/src",
        "grit-cli/src",
        "grit-protocol/src",
        "grit-http-server/src",
        "grit-examples/src",
        "grit-utils/src",
    ]
    .into_iter()
    .map(|rel| root.join(rel))
    .filter(|p| p.is_dir())
    .collect()
}

#[test]
fn no_direct_pack_or_loose_enumeration_outside_odb_modules() {
    let mut violations = Vec::new();
    for dir in production_scan_roots() {
        scan_rs_files(&dir, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "direct pack-index / loose-path enumeration must go through Odb / ObjectStore:\n{}",
        violations.join("\n")
    );
}
