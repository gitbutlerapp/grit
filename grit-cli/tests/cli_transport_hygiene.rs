//! grit-cli must not import low-level transport entry points directly.

#[test]
fn cli_has_no_transport_logic() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let forbidden = [
        "grit_lib::transport",
        "fetch_remote",
        "http_fetch",
        "fetch_local",
        "push_local",
        "push_remote",
        "push_http",
    ];
    for entry in walk_rs(&src) {
        let text = std::fs::read_to_string(&entry).expect("read source");
        for needle in forbidden {
            assert!(
                !text.contains(needle),
                "{} must not reference `{needle}` — use grit_lib::remote::Remote",
                entry.display()
            );
        }
    }
}

fn walk_rs(root: &std::path::Path) -> Vec<std::path::PathBuf> {
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
