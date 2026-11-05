//! README and crate-level rustdoc must advertise the same major.minor as `CARGO_PKG_VERSION`.

use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn major_minor(version: &str) -> String {
    let mut parts = version.split('.');
    let major = parts.next().unwrap_or("0");
    let minor = parts.next().unwrap_or("0");
    format!("{major}.{minor}")
}

fn read_crate_file(name: &str) -> String {
    std::fs::read_to_string(manifest_dir().join(name)).unwrap_or_else(|e| {
        panic!("read {name}: {e}");
    })
}

/// Every `version = "X.Y"` (or `X.Y.Z`) in a `grit-lib = { … }` line must match crate major.minor.
fn assert_dependency_table_snippets(text: &str, expected: &str, label: &str) {
    for (i, line) in text.lines().enumerate() {
        if !line.contains("grit-lib") {
            continue;
        }
        let Some(rest) = line.split("version = \"").nth(1) else {
            continue;
        };
        let Some(found) = rest.split('"').next() else {
            continue;
        };
        let found_mm = major_minor(found);
        assert_eq!(
            found_mm,
            expected,
            "{label}: line {} advertises grit-lib {found_mm}, expected {expected}: {line}",
            i + 1
        );
    }
}

/// `grit-lib = "X.Y"` shorthand in README must match major.minor.
fn assert_shorthand_dependency_snippet(text: &str, expected: &str) {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("grit-lib = \"") {
            let Some(found) = rest.split('"').next() else {
                continue;
            };
            assert_eq!(
                major_minor(found),
                expected,
                "README shorthand dependency must match crate major.minor: {line}"
            );
        }
    }
}

#[test]
fn crate_docs_dependency_snippet_matches_version() {
    let pkg_version = env!("CARGO_PKG_VERSION");
    let expected = major_minor(pkg_version);

    let readme = read_crate_file("README.md");
    let lib_rs = read_crate_file("src/lib.rs");

    assert_shorthand_dependency_snippet(&readme, &expected);
    assert_dependency_table_snippets(&readme, &expected, "README.md");
    assert_dependency_table_snippets(&lib_rs, &expected, "src/lib.rs");

    let doc_sentence = format!("This documentation is for **grit-lib** {expected}");
    assert!(
        lib_rs.contains(&doc_sentence),
        "src/lib.rs crate docs must include: {doc_sentence:?}"
    );
}
