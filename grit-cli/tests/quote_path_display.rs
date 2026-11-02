//! Control characters in paths must not reach stdout raw (terminal escape injection).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn init_repo(dir: &std::path::Path) {
    assert!(Command::new(GRIT)
        .args(["init", dir.to_str().unwrap()])
        .status()
        .expect("grit init")
        .success());
}

fn config_repo(dir: &std::path::Path) {
    assert!(Command::new(GRIT)
        .args(["config", "user.name", "N"])
        .current_dir(dir)
        .status()
        .unwrap()
        .success());
    assert!(Command::new(GRIT)
        .args(["config", "user.email", "n@x"])
        .current_dir(dir)
        .status()
        .unwrap()
        .success());
}

fn assert_no_raw_esc(bytes: &[u8], context: &str) {
    assert!(!bytes.iter().any(|&b| b == 0x1b), "raw ESC in {context}");
}

fn assert_quoted_esc(text: &str, context: &str) {
    assert!(
        text.contains("\\033"),
        "expected C-style ESC quoting in {context}:\n{text}"
    );
}

#[test]
fn status_and_diff_quote_control_bytes_in_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());

    let evil = "evil\x1b[31mRED\x1b[0m.txt";
    let bell = "bell\x07.txt";
    let ctl = "ctl\x01char.txt";
    std::fs::File::create(dir.path().join(evil)).unwrap();
    std::fs::File::create(dir.path().join(bell)).unwrap();
    std::fs::File::create(dir.path().join(ctl)).unwrap();

    std::fs::File::create(dir.path().join("tick`name.txt")).unwrap();

    for args in [&["status"][..], &["status", "--markdown"][..]] {
        let out = Command::new(GRIT)
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("grit status");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = out.stdout;
        assert_no_raw_esc(&stdout, &format!("status output for {args:?}"));
        let text = String::from_utf8_lossy(&stdout);
        assert_quoted_esc(&text, &format!("status {args:?}"));
        assert!(
            text.contains("\\a"),
            "expected BEL quoting in {args:?}:\n{text}"
        );
        assert!(
            text.contains("\\001"),
            "expected control-byte quoting in {args:?}:\n{text}"
        );
    }

    let md = Command::new(GRIT)
        .args(["status", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("grit status markdown");
    assert!(md.status.success());
    let md_out = String::from_utf8_lossy(&md.stdout);
    assert!(
        md_out.contains("``tick`name.txt``"),
        "expected doubled-backtick inline code for backtick filename:\n{md_out}"
    );
    assert!(
        !md_out.contains("tick\\`name"),
        "backslash-backtick is not valid CommonMark inline code:\n{md_out}"
    );

    let git_short = Command::new("git")
        .args(["status", "--short"])
        .current_dir(dir.path())
        .output()
        .expect("git status");
    assert!(git_short.status.success());
    let git_line = String::from_utf8_lossy(&git_short.stdout);
    assert!(git_line.contains("\\033"));

    assert!(Command::new(GRIT)
        .args(["add", "."])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "funny names"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    std::fs::write(dir.path().join(evil), b"touch\n").unwrap();
    let diff_out = Command::new(GRIT)
        .args(["diff", "HEAD"])
        .current_dir(dir.path())
        .output()
        .expect("grit diff");
    assert!(diff_out.status.success());
    assert_no_raw_esc(&diff_out.stdout, "diff output");
}

#[test]
fn porcelain_commands_quote_control_bytes_in_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    config_repo(dir.path());

    std::fs::write(dir.path().join("base"), "base\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "base"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    let evil = "evil\x1b[31mRED\x1b[0m";
    let newline = "new\nline";
    std::fs::File::create(dir.path().join(evil)).unwrap();
    std::fs::File::create(dir.path().join(newline)).unwrap();

    let add_out = Command::new(GRIT)
        .args(["add"])
        .current_dir(dir.path())
        .output()
        .expect("grit add");
    assert!(add_out.status.success());
    assert_no_raw_esc(&add_out.stdout, "grit add stdout");
    let add_text = String::from_utf8_lossy(&add_out.stdout);
    assert_quoted_esc(&add_text, "grit add");
    assert!(
        add_text.contains("\"new\\nline\"") || add_text.contains("new\\nline"),
        "expected newline quoting in add:\n{add_text}"
    );

    assert!(Command::new(GRIT)
        .args(["mv", newline, "ok"])
        .current_dir(dir.path())
        .output()
        .expect("grit mv")
        .status
        .success());

    let bell = "u\x07";
    std::fs::File::create(dir.path().join(bell)).unwrap();

    for args in [&["clean"][..], &["clean", "-f"][..]] {
        let out = Command::new(GRIT)
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("grit clean");
        assert!(out.status.success());
        assert_no_raw_esc(&out.stdout, &format!("grit clean {args:?}"));
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("remove") || text.contains("Remove") {
            assert!(
                text.contains("\\033") || text.contains("\\007") || text.contains("\\a"),
                "expected quoting in clean output:\n{text}"
            );
        }
    }

    assert!(Command::new(GRIT)
        .args(["add", evil, "ok"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "stage evil"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    std::fs::write(dir.path().join(evil), b"x\n").unwrap();
    let restore_out = Command::new(GRIT)
        .args(["restore", evil])
        .current_dir(dir.path())
        .output()
        .expect("grit restore");
    assert!(restore_out.status.success());
    assert_no_raw_esc(&restore_out.stdout, "grit restore");
    assert_quoted_esc(
        &String::from_utf8_lossy(&restore_out.stdout),
        "grit restore",
    );

    std::fs::write(dir.path().join(evil), b"y\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["add", evil])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let restore_staged = Command::new(GRIT)
        .args(["restore", "--staged", evil])
        .current_dir(dir.path())
        .output()
        .expect("grit restore --staged");
    assert!(restore_staged.status.success());
    assert_no_raw_esc(&restore_staged.stdout, "grit restore --staged");

    std::fs::File::create(dir.path().join(bell)).unwrap();
    assert!(Command::new(GRIT)
        .args(["add", "-f", bell])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let rm_out = Command::new(GRIT)
        .args(["rm", "-f", bell])
        .current_dir(dir.path())
        .output()
        .expect("grit rm");
    assert!(rm_out.status.success());
    assert_no_raw_esc(&rm_out.stdout, "grit rm");

    std::fs::File::create(dir.path().join(bell)).unwrap();
    assert!(Command::new(GRIT)
        .args(["add", "-f", bell])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let rm_cached = Command::new(GRIT)
        .args(["rm", "--cached", "-f", bell])
        .current_dir(dir.path())
        .output()
        .expect("grit rm --cached");
    assert!(rm_cached.status.success());
    assert_no_raw_esc(&rm_cached.stdout, "grit rm --cached");
}

#[test]
fn merge_conflict_lines_quote_control_bytes_in_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    config_repo(dir.path());

    let evil = "e\x1b[31mV";
    std::fs::write(dir.path().join(evil), "start\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-am", "start"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    assert!(Command::new(GRIT)
        .args(["branch", "b"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    assert!(Command::new(GRIT)
        .args(["switch", "b"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    std::fs::write(dir.path().join(evil), "on b\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-am", "on b"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    assert!(Command::new(GRIT)
        .args(["switch", "main"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    std::fs::write(dir.path().join(evil), "on main\n").unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-am", "on main"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());

    let merge_out = Command::new(GRIT)
        .args(["merge", "b"])
        .current_dir(dir.path())
        .output()
        .expect("grit merge b");
    assert!(
        !merge_out.status.success(),
        "merge should stop on conflicts"
    );
    let combined = [merge_out.stdout.as_slice(), merge_out.stderr.as_slice()].concat();
    assert_no_raw_esc(&combined, "grit merge conflict output");
    let text = String::from_utf8_lossy(&combined);
    assert_quoted_esc(&text, "grit merge CONFLICT line");
}
