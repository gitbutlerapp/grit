//! `grit diff` must error on unknown revisions instead of empty success.

use std::process::Command;

fn grit(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {:?} failed", args);
}

fn init_repo_with_two_commits(dir: &std::path::Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "A"]);
    git(dir, &["config", "user.email", "a@x"]);
    std::fs::write(dir.join("f"), "1\n").expect("write f");
    git(dir, &["add", "f"]);
    git(dir, &["commit", "-qm", "one"]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("f"))
        .expect("open f")
        .write_all(b"2\n")
        .expect("append f");
    git(dir, &["commit", "-qam", "two"]);
}

use std::io::Write;

#[test]
fn diff_unknown_revision_or_path_errors_instead_of_empty_success() {
    let dir = tempfile::tempdir().expect("tempdir");
    let init = grit(dir.path(), &["init"]);
    assert_eq!(init.0, 0, "grit init failed: {}", init.2);

    for spec in ["mainn", "does-not-exist"] {
        let (code, stdout, stderr) = grit(dir.path(), &["diff", spec]);
        assert_ne!(
            code, 0,
            "grit diff {spec} should fail (got exit {code}, stdout={stdout:?}, stderr={stderr:?})"
        );
        assert!(
            stderr.contains("did not match")
                || stderr.contains("not found")
                || stderr.contains("revision")
                || stderr.contains("could not resolve"),
            "expected revision/path error for {spec}, stderr={stderr}"
        );
    }
}

#[test]
fn diff_two_positionals_with_one_unknown_revision_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_two_commits(dir.path());

    for rev_args in [
        ["nosuchbranch", "HEAD"],
        ["HEAD", "nosuchbranch"],
        ["nosuchbranch", "HEAD~1"],
    ] {
        let mut args = vec!["diff"];
        args.extend_from_slice(&rev_args);
        let (code, stdout, stderr) = grit(dir.path(), &args);
        assert_ne!(
            code,
            0,
            "grit diff {} should fail (stdout={stdout:?}, stderr={stderr:?})",
            rev_args.join(" ")
        );
        assert!(
            !stdout.contains("No changes."),
            "unexpected empty success for {}: stdout={stdout:?}",
            rev_args.join(" ")
        );
        assert!(
            stderr.contains("could not resolve")
                || stderr.contains("not found")
                || stderr.contains("revision"),
            "stderr for {}: {stderr}",
            rev_args.join(" ")
        );
    }

    let (code, stdout, _stderr) = grit(dir.path(), &["diff", "--json", "nosuchbranch", "HEAD~1"]);
    assert_ne!(
        code, 0,
        "json two-arg unknown rev should fail, got {stdout}"
    );
}

#[test]
fn diff_range_plus_unknown_second_token_errors_on_second() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_two_commits(dir.path());

    let (code, stdout, stderr) = grit(dir.path(), &["diff", "HEAD~1..HEAD", "nosuchbranch"]);
    assert_ne!(code, 0, "range + unknown rev should fail");
    assert!(
        !stdout.contains("No changes."),
        "unexpected empty success: {stdout:?}"
    );
    assert!(
        stderr.contains("nosuchbranch"),
        "error should name the unknown token, not the range: {stderr}"
    );
    assert!(
        !stderr.contains("HEAD~1..HEAD"),
        "error should not blame the valid range: {stderr}"
    );

    let (code, _stdout, stderr) = grit(dir.path(), &["diff", "HEAD~1..HEAD", "f"]);
    assert_eq!(code, 0, "range + known path f should succeed: {stderr}");
}

#[test]
fn diff_commit_and_known_path_still_works() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_two_commits(dir.path());

    let (code, _stdout, stderr) = grit(dir.path(), &["diff", "HEAD~1", "f"]);
    assert_eq!(
        code, 0,
        "grit diff HEAD~1 f should succeed for known path f: {stderr}"
    );

    let (code, stdout, stderr) = grit(dir.path(), &["diff", "HEAD", "f"]);
    assert_eq!(code, 0, "grit diff HEAD f should succeed: {stderr}");
    assert!(
        stdout.contains("f") || stdout.contains("No changes"),
        "expected diff output mentioning f or no changes: {stdout}"
    );
}
