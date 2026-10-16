//! Integration tests for `--markdown` output on key commands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn init_repo(dir: &std::path::Path) {
    let status = Command::new(GRIT)
        .args(["init", dir.to_str().unwrap()])
        .status()
        .expect("grit init");
    assert!(status.success());
}

#[test]
fn log_markdown_uses_commit_bullets_not_json_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
    Command::new(GRIT)
        .args(["add", "README.md"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "first"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let out = Command::new(GRIT)
        .args(["log", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("log");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("- `"),
        "expected commit bullet, got:\n{stdout}"
    );
    assert!(stdout.contains("first"));
    assert!(
        !stdout.contains("**commits**:"),
        "should not use generic JSON field bullets:\n{stdout}"
    );
}

fn eleven_commit_repo(dir: &std::path::Path) {
    init_repo(dir);
    for i in 0..11 {
        std::fs::write(dir.join("a.txt"), format!("v{i}\n")).unwrap();
        assert!(Command::new(GRIT)
            .args(["add", "a.txt"])
            .current_dir(dir)
            .status()
            .unwrap()
            .success());
        assert!(Command::new(GRIT)
            .args(["commit", &format!("commit {i}")])
            .current_dir(dir)
            .status()
            .unwrap()
            .success());
    }
}

#[test]
fn log_human_continuation_emits_full_opaque_cursor() {
    let dir = tempfile::tempdir().expect("tempdir");
    eleven_commit_repo(dir.path());
    let out = Command::new(GRIT)
        .args(["log"])
        .current_dir(dir.path())
        .output()
        .expect("log");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.contains("→ more: grit log --before="))
        .expect("continuation line");
    let cursor = line.split("--before=").nth(1).expect("cursor suffix");
    assert!(
        cursor.starts_with("lw1."),
        "expected opaque cursor, got: {cursor}"
    );
    assert!(cursor.len() > 20, "cursor must not be truncated: {cursor}");
    let next_page = Command::new(GRIT)
        .args(["log", &format!("--before={cursor}")])
        .current_dir(dir.path())
        .output()
        .expect("log page 2");
    assert!(
        next_page.status.success(),
        "second page failed: {}",
        String::from_utf8_lossy(&next_page.stderr)
    );
}

#[test]
fn log_markdown_continuation_emits_full_opaque_cursor() {
    let dir = tempfile::tempdir().expect("tempdir");
    eleven_commit_repo(dir.path());
    let out = Command::new(GRIT)
        .args(["log", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("log");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let prefix = "More history: run `grit log --before=";
    let start = stdout.find(prefix).expect("markdown continuation");
    let rest = &stdout[start + prefix.len()..];
    let cursor = rest.split('`').next().expect("closing backtick");
    assert!(cursor.starts_with("lw1."), "got: {cursor}");
    assert!(cursor.len() > 20, "cursor must not be truncated: {cursor}");
    let next_page = Command::new(GRIT)
        .args(["log", &format!("--before={cursor}")])
        .current_dir(dir.path())
        .output()
        .expect("log page 2");
    assert!(
        next_page.status.success(),
        "second page failed: {}",
        String::from_utf8_lossy(&next_page.stderr)
    );
}

#[test]
fn diff_markdown_uses_fenced_diff_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(
        dir.path().join("hello.txt"),
        "line one\nline two\nline three\n",
    )
    .unwrap();
    Command::new(GRIT)
        .args(["add", "hello.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    Command::new(GRIT)
        .args(["commit", "-m", "add hello"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    std::fs::write(
        dir.path().join("hello.txt"),
        "line one\nchanged\nline three\n",
    )
    .unwrap();
    let out = Command::new(GRIT)
        .args(["diff", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("diff");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("```diff"), "stdout:\n{stdout}");
    assert!(!stdout.contains("**files**:"));

    let patch = stdout
        .split("```diff")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("diff fence body");
    assert!(
        patch.contains("\n-line two\n") || patch.contains("\n-line two"),
        "deletion should be its own line:\n{patch}"
    );
    assert!(
        patch.contains("\n+changed\n") || patch.contains("\n+changed"),
        "addition should be its own line:\n{patch}"
    );
    assert!(
        !patch.contains("-line two+changed"),
        "hunk lines must not be concatenated:\n{patch}"
    );
}

#[test]
fn branch_markdown_lists_branches_without_json_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();
    Command::new(GRIT)
        .args(["add", "f.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    Command::new(GRIT)
        .args(["commit", "-m", "init"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(Command::new(GRIT)
        .args(["branch", "topic"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let out = Command::new(GRIT)
        .args(["branch", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("branch");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("## Branches"));
    assert!(stdout.contains("**`main`** (current)"));
    assert!(stdout.contains("`topic`"));
    assert!(
        !stdout.contains("**branches**:"),
        "should not dump nested JSON:\n{stdout}"
    );
}

#[test]
fn diff_markdown_uses_extended_fence_when_patch_contains_triple_backticks() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("doc.md"), "before\n```\nafter\n").unwrap();
    Command::new(GRIT)
        .args(["add", "doc.md"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    Command::new(GRIT)
        .args(["commit", "-m", "add doc"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    std::fs::write(dir.path().join("doc.md"), "before\n```\nchanged\n").unwrap();
    let out = Command::new(GRIT)
        .args(["diff", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("diff");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("````diff"),
        "expected extended fence when patch contains ```:\n{stdout}"
    );
    let body = stdout
        .split("````diff\n")
        .nth(1)
        .and_then(|rest| rest.split("\n````").next())
        .expect("extended diff fence body");
    assert!(
        body.contains("+changed"),
        "lines after inner ``` must stay inside the fence:\n{body}"
    );
}

#[test]
fn show_markdown_escapes_pipe_characters_in_diffstat_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    let path = dir.path().join("a|b.txt");
    std::fs::write(&path, "x\n").unwrap();
    Command::new(GRIT)
        .args(["add"])
        .arg(path.to_str().unwrap())
        .current_dir(dir.path())
        .status()
        .unwrap();
    Command::new(GRIT)
        .args(["commit", "-m", "pipe name"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    std::fs::write(&path, "xy\n").unwrap();
    Command::new(GRIT)
        .args(["add"])
        .arg(path.to_str().unwrap())
        .current_dir(dir.path())
        .status()
        .unwrap();
    Command::new(GRIT)
        .args(["commit", "-m", "change"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    let out = Command::new(GRIT)
        .args(["show", "--markdown", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("show");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("a\\|b.txt"),
        "pipe in filename must be escaped in the table row:\n{stdout}"
    );
    assert!(
        stdout.contains("| `a\\|b.txt` |"),
        "table row must keep the filename in one cell:\n{stdout}"
    );
    assert!(
        !stdout.contains("| `a|b.txt` |"),
        "unescaped pipe must not appear in the table row:\n{stdout}"
    );
}

#[test]
fn status_markdown_hints_use_inline_code_not_html_tags() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("new.txt"), "n\n").unwrap();
    let out = Command::new(GRIT)
        .args(["status", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("status");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("grit add `PATH` to stage"));
    assert!(!stdout.contains("<file>"));
}

#[test]
fn status_markdown_has_staged_section() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("x.txt"), "x\n").unwrap();
    Command::new(GRIT)
        .args(["add", "x.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    let out = Command::new(GRIT)
        .args(["status", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("status");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("## Staged"));
    assert!(stdout.contains("x.txt"));
}

#[test]
fn skill_markdown_prints_verbatim_skill_file() {
    let out = Command::new(GRIT)
        .args(["skill", "--markdown"])
        .output()
        .expect("skill");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("---\nname: grit\n"));
    assert!(!stdout.contains("**content**:"));
}

#[test]
fn blame_markdown_table_rows_are_single_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), "one\r\n").unwrap();
    Command::new(GRIT)
        .args(["add", "f.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    Command::new(GRIT)
        .args(["commit", "-m", "init"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    let out = Command::new(GRIT)
        .args(["blame", "f.txt", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("blame");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if line.starts_with("| ") && line.chars().nth(2).is_some_and(|c| c.is_ascii_digit()) {
            assert!(
                line.ends_with(" |"),
                "table data row must be one line ending with ' |':\n{line}\nfull stdout:\n{stdout}"
            );
            assert!(
                !line.contains('\r'),
                "table row must not contain raw carriage returns:\n{line}"
            );
        }
    }
    let json = Command::new(GRIT)
        .args(["blame", "f.txt", "--json"])
        .current_dir(dir.path())
        .output()
        .expect("blame json");
    assert!(json.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&json.stdout).expect("blame json parse");
    for line in parsed["lines"].as_array().expect("lines array") {
        let content = line["content"].as_str().expect("content");
        assert!(
            !content.ends_with('\n') && !content.ends_with('\r'),
            "json content must not trail line terminators: {content:?}"
        );
    }
}

#[test]
fn clone_markdown_lists_stats() {
    let base = tempfile::tempdir().expect("tempdir");
    let origin = base.path().join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    let git = |args: &[&str], cwd: &std::path::Path| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "--bare", origin.to_str().unwrap()], base.path());
    git(
        &["clone", "-q", origin.to_str().unwrap(), "seed"],
        base.path(),
    );
    let seed = base.path().join("seed");
    std::fs::write(seed.join("f.txt"), "x\n").unwrap();
    git(&["add", "f.txt"], &seed);
    git(
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e.com",
            "commit",
            "-m",
            "c",
        ],
        &seed,
    );
    assert!(Command::new(GRIT)
        .args(["push"])
        .current_dir(&seed)
        .status()
        .unwrap()
        .success());

    let clone_dir = base.path().join("cloned");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--markdown",
            origin.to_str().unwrap(),
            clone_dir.to_str().unwrap(),
        ])
        .current_dir(base.path())
        .output()
        .expect("clone");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for key in [
        "**objects**",
        "**bytes**",
        "**branches**",
        "**tags**",
        "**files**",
    ] {
        assert!(stdout.contains(key), "missing {key} in:\n{stdout}");
    }
}

#[test]
fn completions_rejects_json_and_markdown() {
    for flag in ["--json", "--markdown"] {
        let out = Command::new(GRIT)
            .args(["completions", "zsh", flag])
            .output()
            .expect("completions");
        assert!(!out.status.success(), "expected failure with {flag}");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(combined.contains("does not support"));
    }
}
