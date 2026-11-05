//! Embedders (`grit-cli`, `grit-examples`) must not use struct-update syntax on
//! [`grit_lib::porcelain::add::StageOptions`]: test-only fields are `pub(crate)` without
//! the `test-support` feature, so `..StageOptions::default()` fails to compile.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("grit-lib has a parent directory")
        .to_path_buf()
}

fn scan_rs_files(dir: &Path, violations: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "target" || n == "tests")
            {
                continue;
            }
            scan_rs_files(&path, violations);
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        if contents.contains("..StageOptions::default()") {
            violations.push(format!(
                "{}: use `StageOptions::default()` and assign public fields instead of struct-update syntax",
                path.display()
            ));
        }
    }
}

#[test]
fn embedders_do_not_struct_update_stage_options() {
    let root = workspace_root();
    let mut violations = Vec::new();
    for rel in ["grit-cli/src", "grit-examples"] {
        let dir = root.join(rel);
        if dir.is_dir() {
            scan_rs_files(&dir, &mut violations);
        }
    }
    assert!(
        violations.is_empty(),
        "StageOptions struct-update breaks default-feature builds:\n{}",
        violations.join("\n")
    );
}
