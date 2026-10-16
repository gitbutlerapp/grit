//! Issue #1109: merge and stash-pop conflict markers must be smudged to CRLF when
//! `.gitattributes` sets `eol=crlf` (same as non-conflicted checkout paths).

use std::fs;
use std::io::Write;

use grit_test_support::{git, grit, grit_cmd};

fn git_eol_lines(repo: &std::path::Path, path: &str) -> Vec<String> {
    git(repo, &["ls-files", "--eol", path])
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn issue_1109_merge_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    grit_cmd(&["init"]).in_dir(root).suc();
    git(root, &["config", "user.name", "V"]);
    git(root, &["config", "user.email", "v@x.test"]);
    fs::write(root.join(".gitattributes"), "*.txt text eol=crlf\n").expect("attrs");
    fs::write(root.join("f.txt"), "a\nb\nc\n").expect("f");
    fs::write(root.join("g.txt"), "x\ny\n").expect("g");
    assert!(grit(root, &["add"]).ok(), "grit add");
    assert!(grit(root, &["commit", "-m", "base"]).ok(), "base commit");
    assert!(grit(root, &["branch", "s"]).ok(), "branch s");

    fs::write(root.join("f.txt"), "a\nMAIN\nc\n").expect("main f");
    fs::write(root.join("g.txt"), "x\ny\nmain-g\n").expect("main g");
    assert!(grit(root, &["commit", "-m", "m"]).ok(), "main commit");

    assert!(grit(root, &["switch", "s"]).ok(), "switch s");
    fs::write(root.join("f.txt"), "a\nSIDE\nc\n").expect("side f");
    assert!(grit(root, &["commit", "-m", "s"]).ok(), "side commit");

    assert!(grit(root, &["switch", "main"]).ok(), "switch main");
    tmp
}

#[test]
fn merge_conflict_markers_smudge_to_crlf_under_eol_crlf() {
    let tmp = issue_1109_merge_repo();
    let root = tmp.path();

    let merge_out = grit(root, &["merge", "s"]);
    assert!(
        !merge_out.ok(),
        "merge should stop with conflicts: {}",
        merge_out.dump("merge s")
    );

    let f_bytes = fs::read(root.join("f.txt")).expect("read f.txt");
    assert!(
        f_bytes.windows(2).any(|w| w == b"\r\n"),
        "conflicted f.txt must use CRLF in worktree, got LF-only body"
    );
    assert!(
        f_bytes.starts_with(b"a\r\n"),
        "first line must be CRLF-smudged"
    );
    let body = String::from_utf8_lossy(&f_bytes);
    assert!(body.contains("<<<<<<<"), "expected conflict markers");
    assert!(
        body.contains("<<<<<<< HEAD\r\n"),
        "marker lines must be CRLF-terminated like git merge"
    );

    let g_bytes = fs::read(root.join("g.txt")).expect("read g.txt");
    assert_eq!(
        g_bytes, b"x\r\ny\r\nmain-g\r\n",
        "non-conflict path should stay CRLF-smudged"
    );

    for line in git_eol_lines(root, "f.txt") {
        assert!(
            line.contains("w/crlf"),
            "every unmerged f.txt stage should report w/crlf, got: {line:?}"
        );
    }
    let g_eol = git_eol_lines(root, "g.txt");
    assert!(
        g_eol.iter().any(|l| l.contains("w/crlf")),
        "g.txt should report w/crlf, got: {g_eol:?}"
    );
}

#[test]
fn stash_pop_conflict_markers_smudge_to_crlf_under_eol_crlf() {
    let tmp = issue_1109_merge_repo();
    let root = tmp.path();

    let merge_out = grit(root, &["merge", "s"]);
    assert!(!merge_out.ok(), "setup merge conflict");

    assert!(grit(root, &["merge", "--abort"]).ok(), "merge abort");

    fs::OpenOptions::new()
        .append(true)
        .open(root.join("g.txt"))
        .expect("open g")
        .write_all(b"A\n")
        .expect("append A");
    assert!(grit(root, &["stash"]).ok(), "stash");

    fs::OpenOptions::new()
        .append(true)
        .open(root.join("g.txt"))
        .expect("open g")
        .write_all(b"B\n")
        .expect("append B");
    assert!(grit(root, &["commit", "-m", "b"]).ok(), "commit b");

    let pop_out = grit(root, &["stash", "pop"]);
    assert!(
        !pop_out.ok(),
        "stash pop should report conflict: {}",
        pop_out.dump("stash pop")
    );

    let g_bytes = fs::read(root.join("g.txt")).expect("read g.txt");
    assert!(
        g_bytes.windows(2).filter(|w| *w == b"\r\n").count() >= 4,
        "conflicted g.txt after stash pop must be CRLF throughout"
    );
    let body = String::from_utf8_lossy(&g_bytes);
    assert!(
        body.contains("<<<<<<<") && body.contains("\r\n"),
        "stash conflict markers must use CRLF line endings"
    );

    for line in git_eol_lines(root, "g.txt") {
        if line.contains("unmerged") || line.chars().any(|c| c.is_ascii_digit()) {
            assert!(
                line.contains("w/crlf"),
                "unmerged g.txt stage should report w/crlf, got: {line:?}"
            );
        }
    }
}
