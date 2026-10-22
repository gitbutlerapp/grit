//! Workspace guard: only `grit-lib/src/hash*.rs` and `grit-test-support/src/objects/`
//! may reference the `sha1` / `sha2` crates (pack fixture hashing for tests).

use std::path::{Path, PathBuf};

fn forbidden_tokens() -> [&'static str; 4] {
    [
        concat!("sha", "1", "::"),
        concat!("sha", "2", "::"),
        concat!("Sha", "1", "::"),
        concat!("Sha", "256", "::"),
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
    path_str.contains("/grit-lib/src/hash")
        || path_str.contains("\\grit-lib\\src\\hash")
        || path_str.contains("/grit-test-support/src/objects/")
        || path_str.contains("\\grit-test-support\\src\\objects\\")
        || path
            .file_name()
            .is_some_and(|n| n == "hash_abstraction_guard.rs")
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

/// Production code must not compute file/checksum trailers via [`ObjectHasher`].
#[test]
fn no_object_hasher_for_file_checksums_in_src() {
    let grit_lib_src = workspace_root().join("grit-lib/src");
    let mut violations = Vec::new();
    scan_checksum_hasher_violations(&grit_lib_src, &mut violations);
    assert!(
        violations.is_empty(),
        "ObjectHasher / HashAlgo::hasher() belongs on object-id paths only (use ChecksumHasher for trailers):\n{}",
        violations.join("\n")
    );
}

fn scan_checksum_hasher_violations(dir: &Path, violations: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_checksum_hasher_violations(&path, violations);
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let path_str = path.to_string_lossy();
        if path_str.contains("/hash/") || path_str.contains("\\hash\\") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read rust source");
        for (line_no, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if line.contains("ObjectHasher::") || line.contains(".hasher()") {
                violations.push(format!("{}:{}: {}", path.display(), line_no + 1, trimmed));
            }
        }
    }
}

#[test]
fn no_direct_sha_crate_use_outside_hash_module() {
    let mut violations = Vec::new();
    scan_rs_files(&workspace_root(), &mut violations);
    assert!(
        violations.is_empty(),
        "direct sha1/sha2 crate use must go through grit_lib::objects::hash:\n{}",
        violations.join("\n")
    );
}
