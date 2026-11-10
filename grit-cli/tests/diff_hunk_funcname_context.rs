//! Hunk `context` in structured diff output follows `.gitattributes` diff drivers.

use std::fs;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn cpp_goto_fixture(dir: &TempDir) {
    fs::write(dir.path().join(".gitattributes"), "*.c diff=cpp\n").expect("attrs");
    let body = "int f(int x)\n{\n\tif (x)\n\t\tgoto out;\n\tx++;\nout:\n\treturn x;\n}\n";
    fs::write(dir.path().join("a.c"), body).expect("write");
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    git_in(dir.path(), &["add", "-A"]);
    git_in(dir.path(), &["commit", "-q", "-m", "init"]);
    fs::write(
        dir.path().join("a.c"),
        body.replace("return x;", "return x + 1;"),
    )
    .expect("modify");
}

fn grit_stdout(dir: &std::path::Path, args: &[&str]) -> String {
    let grit = env!("CARGO_BIN_EXE_grit");
    let out = Command::new(grit)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn grit");
    assert!(
        out.status.success(),
        "grit {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn diff_json_hunk_context_uses_gitattributes_driver() {
    let dir = TempDir::new().expect("tempdir");
    cpp_goto_fixture(&dir);
    let json = grit_stdout(dir.path(), &["diff", "--json"]);
    let v: Value = serde_json::from_str(&json).expect("json");
    let ctx = v["files"][0]["hunks"][0]["context"]
        .as_str()
        .expect("hunk context");
    assert_eq!(ctx, "int f(int x)");
}

#[test]
fn diff_markdown_hunk_context_uses_gitattributes_driver() {
    let dir = TempDir::new().expect("tempdir");
    cpp_goto_fixture(&dir);
    let md = grit_stdout(dir.path(), &["diff", "--markdown"]);
    assert!(
        md.contains("@@ -4,3 +4,3 @@ int f(int x)")
            || md.contains("@@ -") && md.contains("int f(int x)"),
        "markdown hunk header should include cpp funcname:\n{md}"
    );
}

#[test]
fn diff_human_hunk_context_uses_gitattributes_driver() {
    let dir = TempDir::new().expect("tempdir");
    cpp_goto_fixture(&dir);
    let human = grit_stdout(dir.path(), &["diff"]);
    let hunk_headers: Vec<&str> = human.lines().filter(|line| line.contains("@@")).collect();
    assert!(
        hunk_headers
            .iter()
            .any(|line| line.contains("int f(int x)")),
        "human hunk header should include cpp funcname:\n{human}"
    );
    assert!(
        !hunk_headers.iter().any(|line| line.contains("out:")),
        "must not treat goto label as funcname in hunk header:\n{human}"
    );
}
