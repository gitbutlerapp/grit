//! `grit commit --dry-run` CLI and JSON output.

use std::error::Error;
use std::fs;
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn null_config() -> String {
    if cfg!(windows) {
        "NUL".to_string()
    } else {
        "/dev/null".to_string()
    }
}

fn run_grit(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let null = null_config();
    Command::new(GRIT)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("grit")
}

#[test]
fn commit_dry_run_json_omits_oid_and_lists_files() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert!(run_grit(dir.path(), &["init"]).status.success());
    fs::write(dir.path().join("a.txt"), b"a\n")?;
    assert!(run_grit(dir.path(), &["commit", "-m", "init"])
        .status
        .success());
    fs::write(dir.path().join("a.txt"), b"b\n")?;

    let head_before = fs::read_to_string(dir.path().join(".git/HEAD"))?;
    let out = run_grit(
        dir.path(),
        &["commit", "--dry-run", "-m", "preview", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(v.get("dry_run").and_then(Value::as_bool), Some(true));
    assert!(v.get("oid").is_none());
    assert_eq!(
        v.get("files").and_then(Value::as_array).map(Vec::len),
        Some(1)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".git/HEAD"))?,
        head_before
    );
    Ok(())
}

#[test]
fn commit_dry_run_human_preview() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert!(run_grit(dir.path(), &["init"]).status.success());
    fs::write(dir.path().join("f.txt"), b"1\n")?;
    assert!(run_grit(dir.path(), &["commit", "-m", "init"])
        .status
        .success());
    fs::write(dir.path().join("f.txt"), b"2\n")?;

    let out = run_grit(dir.path(), &["commit", "--dry-run", "-m", "next"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Would commit"));
    assert!(stdout.contains("f.txt"));
    Ok(())
}

#[test]
fn commit_dry_run_missing_message_does_not_persist_index() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert!(run_grit(dir.path(), &["init"]).status.success());
    fs::write(dir.path().join("a.txt"), b"a\n")?;
    assert!(run_grit(dir.path(), &["commit", "-m", "init"])
        .status
        .success());
    fs::write(dir.path().join("a.txt"), b"b\n")?;

    let index_before = fs::metadata(dir.path().join(".git/index"))?.modified()?;
    let out = run_grit(dir.path(), &["commit", "--dry-run"]);
    assert!(!out.status.success());
    let index_after = fs::metadata(dir.path().join(".git/index"))?.modified()?;
    assert_eq!(index_before, index_after);
    Ok(())
}

#[test]
fn commit_dry_run_amend_without_message_uses_head_subject() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert!(run_grit(dir.path(), &["init"]).status.success());
    fs::write(dir.path().join("a.txt"), b"a\n")?;
    assert!(run_grit(dir.path(), &["commit", "-m", "keep subject"])
        .status
        .success());
    fs::write(dir.path().join("a.txt"), b"b\n")?;

    let out = run_grit(dir.path(), &["commit", "--dry-run", "--amend", "--json"]);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(
        v.get("subject").and_then(Value::as_str),
        Some("keep subject")
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn parallel_symlink_commit_preview_matches_commit_without_writes() -> TestResult {
    use grit_test_support::git;
    use std::os::unix::fs::symlink;

    fn snapshot(dir: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        for entry in fs::read_dir(dir).expect("snapshot directory") {
            let path = entry.expect("snapshot entry").path();
            if path.is_dir() {
                files.extend(snapshot(&path));
            } else {
                files.insert(path.clone(), fs::read(path).expect("snapshot file"));
            }
        }
        files
    }

    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);
    fs::create_dir(root.join("files"))?;
    for n in 0..1024 {
        fs::write(root.join(format!("files/{n}")), b"data\n")?;
    }
    for (name, target) in [
        ("linkdir", "files"),
        ("linkfile", "files/0"),
        ("linkmissing", "missing"),
    ] {
        symlink(target, root.join(name))?;
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    git(root, &["read-tree", "HEAD"]);
    let before = snapshot(&root.join(".git"));
    let index_mtime = fs::metadata(root.join(".git/index"))?.modified()?;
    for threads in ["1", "4"] {
        let out = run_grit(
            root,
            &[
                "--threads",
                threads,
                "commit",
                "--dry-run",
                "-m",
                "clean",
                "--json",
            ],
        );
        assert!(!out.status.success());
        let json: Value = serde_json::from_slice(&out.stdout)?;
        assert_eq!(json["kind"], "nothing_to_commit", "{json}");
        assert_eq!(snapshot(&root.join(".git")), before);
        assert_eq!(
            fs::metadata(root.join(".git/index"))?.modified()?,
            index_mtime
        );
    }
    for (name, target) in [
        ("linkdir", "."),
        ("linkfile", "files/1"),
        ("linkmissing", "absent"),
    ] {
        fs::remove_file(root.join(name))?;
        symlink(target, root.join(name))?;
    }
    let mut preview_files = None;
    for threads in ["1", "4"] {
        let out = run_grit(
            root,
            &[
                "--threads",
                threads,
                "commit",
                "--dry-run",
                "-m",
                "retarget",
                "--json",
            ],
        );
        assert!(
            out.status.success(),
            "{} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let json: Value = serde_json::from_slice(&out.stdout)?;
        assert_eq!(json["changes"], 3);
        assert_eq!(json["files"].as_array().expect("files").len(), 3);
        if let Some(expected) = &preview_files {
            assert_eq!(&json["files"], expected);
        }
        preview_files = Some(json["files"].clone());
        assert_eq!(snapshot(&root.join(".git")), before);
        assert_eq!(
            fs::metadata(root.join(".git/index"))?.modified()?,
            index_mtime
        );
    }
    let out = run_grit(
        root,
        &["--threads", "4", "commit", "-m", "retarget", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let committed: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(Some(committed["files"].clone()), preview_files);
    for (name, target) in [
        ("linkdir", "."),
        ("linkfile", "files/1"),
        ("linkmissing", "absent"),
    ] {
        assert_eq!(git(root, &["show", &format!("HEAD:{name}")]), target);
        assert!(git(root, &["ls-tree", "HEAD", name]).starts_with("120000"));
    }
    assert!(git(root, &["diff"]).is_empty());
    git(root, &["fsck", "--strict"]);
    Ok(())
}
