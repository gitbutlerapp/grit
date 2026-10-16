//! Workspace guard: grit-protocol must not shell out to `grit` or other programs.

use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn grit_protocol_never_spawns() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    for path in files {
        let text = fs::read_to_string(&path).expect("read source");
        assert!(
            !text.contains("std::process"),
            "forbidden subprocess API in {}",
            path.display()
        );
        assert!(
            !text.contains("Command::new"),
            "forbidden Command spawn in {}",
            path.display()
        );
    }
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read src dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}
