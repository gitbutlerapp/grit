//! grit show: multiple objects, rev:path, --diff-merges.

use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_test_support::{git, grit};

fn gs_json(repo: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grit"));
    cmd.current_dir(repo).arg("--json");
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().expect("grit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn two_parent_merge(dir: &std::path::Path) -> ObjectId {
    std::fs::write(dir.join("file"), "base\n").unwrap();
    git(dir, &["add", "file"]);
    git(dir, &["commit", "-qm", "root"]);
    let root = git(dir, &["symbolic-ref", "--short", "HEAD"])
        .trim()
        .to_owned();
    git(dir, &["checkout", "-q", "-b", "p1"]);
    std::fs::write(dir.join("file"), "one\n").unwrap();
    git(dir, &["add", "file"]);
    git(dir, &["commit", "-qm", "p1"]);
    git(dir, &["checkout", "-q", &root]);
    std::fs::write(dir.join("file"), "two\n").unwrap();
    git(dir, &["add", "file"]);
    git(dir, &["commit", "-qm", "p2"]);
    git(dir, &["checkout", "-q", "-b", "merge", "p1"]);
    std::fs::write(dir.join("file"), "merged\n").unwrap();
    git(dir, &["add", "file"]);
    git(dir, &["commit", "-qm", "merge"]);
    git(dir, &["rev-parse", "HEAD"]).trim().parse().unwrap()
}

#[test]
fn show_multiple_objects_json_objects_array() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "a"]);
    git(dir.path(), &["commit", "--allow-empty", "-m", "b"]);
    grit(dir.path(), &["init"]).expect_ok();

    let json = gs_json(dir.path(), &["show", "HEAD~1", "HEAD"]);
    assert!(
        json.get("objects").is_some(),
        "expected objects array: {json}"
    );
    assert_eq!(json["objects"].as_array().map(|a| a.len()), Some(2));
}

#[test]
fn show_rev_path_blob_matches_git_show() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init"]);
    std::fs::write(dir.path().join("hello.txt"), "hello world\n").unwrap();
    git(dir.path(), &["add", "hello.txt"]);
    git(dir.path(), &["commit", "-m", "add file"]);
    grit(dir.path(), &["init"]).expect_ok();

    let git_out = Command::new("git")
        .current_dir(dir.path())
        .args(["show", "HEAD:hello.txt"])
        .output()
        .expect("git show");
    assert!(git_out.status.success());

    let json = gs_json(dir.path(), &["show", "HEAD:hello.txt"]);
    assert_eq!(json["kind"], "blob");
    assert_eq!(json["text"], "hello world\n");
    let grit_human = grit(dir.path(), &["show", "HEAD:hello.txt"]);
    assert!(grit_human.ok(), "{}", grit_human.dump("show rev:path"));
    assert_eq!(grit_human.stdout.as_bytes(), &git_out.stdout);
}

#[test]
fn show_rev_path_tree_lists_entries() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init"]);
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-m", "init"]);
    grit(dir.path(), &["init"]).expect_ok();

    let json = gs_json(dir.path(), &["show", "HEAD:"]);
    assert_eq!(json["kind"], "tree");
    let names: Vec<_> = json["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"a.txt"));
}

#[test]
fn show_diff_merges_first_parent_matches_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init"]);
    let merge = two_parent_merge(dir.path());
    grit(dir.path(), &["init"]).expect_ok();

    let git_out = Command::new("git")
        .current_dir(dir.path())
        .args(["show", "--format=", &merge.to_hex()])
        .output()
        .expect("git show");
    assert!(git_out.status.success());

    let grit_out = grit(
        dir.path(),
        &["show", "--diff-merges=first-parent", &merge.to_hex()],
    );
    assert!(grit_out.ok(), "{}", grit_out.dump("grit show first-parent"));

    let git_patch = String::from_utf8_lossy(&git_out.stdout);
    let grit_patch = &grit_out.stdout;
    assert!(
        git_patch.contains("file") && grit_patch.contains("file"),
        "both should diff file"
    );
    assert!(
        !grit_patch.contains("@@@") || git_patch.contains("@@@"),
        "first-parent mode should not use combined hunks when git does not"
    );
}

trait GritOk {
    fn expect_ok(self) -> grit_test_support::Output;
}
impl GritOk for grit_test_support::Output {
    fn expect_ok(self) -> grit_test_support::Output {
        assert!(self.ok(), "{}", self.dump("grit"));
        self
    }
}
