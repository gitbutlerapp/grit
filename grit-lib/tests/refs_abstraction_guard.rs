//! Workspace guard: ref backend selection and on-disk path layout stay inside the ref store layer.

use std::path::{Path, PathBuf};

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
        .is_some_and(|n| n == "refs_abstraction_guard.rs" || n == "odb_abstraction_guard.rs")
    {
        return true;
    }

    const REF_STORE_TREE: &[&str] = &[
        "/grit-lib/src/refs/store/",
        "\\grit-lib\\src\\refs\\store\\",
    ];
    if REF_STORE_TREE.iter().any(|p| path_str.contains(p)) {
        return true;
    }

    const SELECTION_AND_FACADE: &[&str] = &[
        "/grit-lib/src/ref_storage.rs",
        "\\grit-lib\\src\\ref_storage.rs",
        "/grit-lib/src/refs/mod.rs",
        "\\grit-lib\\src\\refs\\mod.rs",
        "/grit-lib/src/reftable.rs",
        "\\grit-lib\\src\\reftable.rs",
        "/grit-lib/src/reflog.rs",
        "\\grit-lib\\src\\reflog.rs",
        "/grit-lib/src/refs_fsck.rs",
        "\\grit-lib\\src\\refs_fsck.rs",
        "/grit-lib/src/git_path.rs",
        "\\grit-lib\\src\\git_path.rs",
        "/grit-lib/src/repo_caches.rs",
        "\\grit-lib\\src\\repo_caches.rs",
    ];
    if SELECTION_AND_FACADE.iter().any(|p| path_str.contains(p)) {
        return true;
    }

    const PACKED_REFS_EXTRA: &[&str] = &[
        "/grit-lib/src/remote.rs",
        "\\grit-lib\\src\\remote.rs",
        "/grit-lib/src/commit_graph_write.rs",
        "\\grit-lib\\src\\commit_graph_write.rs",
        "/grit-lib/src/repo.rs",
        "\\grit-lib\\src\\repo.rs",
        "/grit-lib/src/merge_base.rs",
        "\\grit-lib\\src\\merge_base.rs",
    ];
    if PACKED_REFS_EXTRA.iter().any(|p| path_str.contains(p)) {
        return true;
    }

    if path_str.contains("/grit-lib/tests/") || path_str.contains("\\grit-lib\\tests\\") {
        return true;
    }
    if path_str.contains("/grit-lib/benches/") || path_str.contains("\\grit-lib\\benches\\") {
        return true;
    }

    path_str.contains("/grit-lib/src/repository_config_snapshot_tests.rs")
        || path_str.contains("\\grit-lib\\src\\repository_config_snapshot_tests.rs")
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
                if line.contains(&legacy_reftable_repo_probe_token()) {
                    violations.push(format!(
                        "{}:{}: forbidden legacy reftable-repo probe",
                        path.display(),
                        line_no + 1
                    ));
                }
                if line.contains(&legacy_reftable_config_probe_token()) {
                    violations.push(format!(
                        "{}:{}: forbidden legacy reftable config probe",
                        path.display(),
                        line_no + 1
                    ));
                }
                if line.contains("RefStorageFormat::detect(")
                    || line.contains("RefStorageFormat::parse_config_value(")
                {
                    violations.push(format!(
                        "{}:{}: ref storage format detection/parsing outside selection",
                        path.display(),
                        line_no + 1
                    ));
                }
                if line.contains(".join(\"packed-refs\")") {
                    violations.push(format!(
                        "{}:{}: direct `packed-refs` path join",
                        path.display(),
                        line_no + 1
                    ));
                }
                if line.contains(".join(\"logs\")") && line.contains("refs") {
                    violations.push(format!(
                        "{}:{}: direct reflog path join under logs/refs",
                        path.display(),
                        line_no + 1
                    ));
                }
                if line.contains("\"logs/refs") {
                    violations.push(format!(
                        "{}:{}: literal logs/refs path outside ref store",
                        path.display(),
                        line_no + 1
                    ));
                }
            }
        }
    }
}

fn legacy_reftable_repo_probe_token() -> String {
    String::from_utf8(
        [
            105, 115, 95, 114, 101, 102, 116, 97, 98, 108, 101, 95, 114, 101, 112, 111,
        ]
        .to_vec(),
    )
    .expect("ascii token")
}

fn legacy_reftable_config_probe_token() -> String {
    String::from_utf8(
        [
            114, 101, 102, 116, 97, 98, 108, 101, 95, 100, 101, 99, 108, 97, 114, 101, 100, 95,
            105, 110, 95, 114, 101, 112, 111, 115, 105, 116, 111, 114, 121, 95, 99, 111, 110, 102,
            105, 103,
        ]
        .to_vec(),
    )
    .expect("ascii token")
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
fn no_ad_hoc_ref_backend_probes_outside_store_layer() {
    let mut violations = Vec::new();
    for dir in production_scan_roots() {
        scan_rs_files(&dir, &mut violations);
    }
    assert!(
        violations.is_empty(),
        "ref backend selection and on-disk ref paths must go through refs/store and ref_storage:\n{}",
        violations.join("\n")
    );
}
