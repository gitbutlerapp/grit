//! CLI wiring for `-M`, `--no-renames`, `--diff-algorithm`, and `--diff-filter`.

use std::process::Command;

fn grit(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(args)
        .output()
        .expect("run grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn diff_rename_algorithm_filter_flags() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "diff.algorithm", "patience"]);
    std::fs::write(root.join("a.txt"), "line one\nline two\n").unwrap();
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "base"]);
    std::fs::remove_file(root.join("a.txt")).unwrap();
    std::fs::write(root.join("b.txt"), "line one\nline two\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "rename"]);

    let (code, _, stderr) = grit(root, &["diff", "--no-renames", "HEAD~1", "HEAD", "--json"]);
    assert_eq!(code, 0, "stderr={stderr}");

    let (code, stdout, stderr) = grit(
        root,
        &[
            "diff",
            "--no-renames",
            "--diff-filter=AD",
            "HEAD~1",
            "HEAD",
            "--json",
        ],
    );
    assert_eq!(code, 0, "stderr={stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let paths: Vec<_> = json["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    assert!(paths.contains(&"b.txt".to_string()));
    assert!(paths.contains(&"a.txt".to_string()));

    let (code, _, stderr) = grit(root, &["diff", "-M30", "HEAD~1", "HEAD", "--json"]);
    assert_eq!(code, 0, "stderr={stderr}");

    let (code, _, stderr) = grit(
        root,
        &["diff", "--diff-algorithm", "histogram", "HEAD~1", "HEAD"],
    );
    assert_eq!(code, 0, "stderr={stderr}");

    let (code, _, stderr) = grit(root, &["diff", "--diff-filter=Z", "HEAD~1", "HEAD"]);
    assert_ne!(code, 0, "invalid filter should fail");
    assert!(stderr.contains("unknown") || stderr.contains("filter"));
}
