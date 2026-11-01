//! `grit diff` must emit Git-style stderr when inexact rename detection is skipped.

use grit_test_support::{git, grit};

#[test]
fn commit_diff_emits_inexact_rename_limit_notice_on_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), "alpha one\n").expect("write a");
    std::fs::write(dir.path().join("b.txt"), "beta two\n").expect("write b");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "base"]);

    std::fs::remove_file(dir.path().join("a.txt")).expect("rm a");
    std::fs::remove_file(dir.path().join("b.txt")).expect("rm b");
    std::fs::write(dir.path().join("x.txt"), "alpha one edited\n").expect("write x");
    std::fs::write(dir.path().join("y.txt"), "beta two edited\n").expect("write y");
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "renamed"]);

    let grit_init = grit(dir.path(), &["init"]);
    assert!(grit_init.ok(), "{}", grit_init.dump("grit init"));

    for args in [
        &["diff", "HEAD~1", "HEAD", "--name-only"][..],
        &["diff", "HEAD~1", "HEAD", "--json"][..],
        &["diff", "HEAD~1", "HEAD"][..],
    ] {
        let out = grit(
            dir.path(),
            &["-c", "diff.renameLimit=1", "-c", "diff.renames=true"]
                .into_iter()
                .chain(args.iter().copied())
                .collect::<Vec<_>>(),
        );
        assert!(out.ok(), "{}: {}", args.join(" "), out.dump("grit diff"));
        let stderr = &out.stderr;
        assert!(
            stderr.contains("skipped inexact rename detection"),
            "expected stderr notice for {args:?}, got: {stderr}"
        );
        assert!(
            stderr.contains("diff.renameLimit=1"),
            "stderr should cite limit: {stderr}"
        );
    }

    let git_out = std::process::Command::new("git")
        .current_dir(dir.path())
        .args([
            "-c",
            "diff.renameLimit=1",
            "-c",
            "diff.renames=true",
            "diff",
            "HEAD~1",
            "HEAD",
        ])
        .output()
        .expect("git diff");
    let git_stderr = String::from_utf8_lossy(&git_out.stderr);
    assert!(
        git_stderr.contains("rename detection was skipped")
            || git_stderr.contains("renameLimit"),
        "git should warn for fixture: {git_stderr}"
    );
}
