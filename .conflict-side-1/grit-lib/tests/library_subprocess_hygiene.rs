//! Task hygiene: grit-lib must not shell out to grep/iconv/kill/stty.

#[test]
fn grit_lib_src_has_no_forbidden_subprocess_helpers() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let forbidden = [
        "Command::new(\"grep\")",
        "Command::new(\"iconv\")",
        "Command::new(\"kill\")",
        "Command::new(\"stty\")",
    ];
    for entry in walkdir_light(&src) {
        let text = std::fs::read_to_string(&entry).expect("read source");
        for needle in forbidden {
            assert!(
                !text.contains(needle),
                "{} must not invoke {needle}",
                entry.display()
            );
        }
    }
}

fn walkdir_light(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out
}
