//! Exit-code documentation contract and CLI `--json` `kind` values.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn grit(dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("grit");
    let code = out.status.code().unwrap_or(-1);
    (code, String::from_utf8_lossy(&out.stdout).into_owned())
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let (code, _) = grit(dir.path(), &["init"]);
    assert_eq!(code, 0);
    dir
}

#[test]
fn exit_codes_page_matches_enum() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../content/docs/exit-codes.md");
    let text = fs::read_to_string(&path).expect("read exit-codes.md");
    for slug in [
        "success",
        "failure",
        "usage",
        "remote_unreadable",
        "missing_prerequisite",
        "refused_not_ff",
        "invalid_commit_filter",
        "custom",
        "conflict",
        "hash_collision",
        "would_overwrite",
        "invalid_pattern",
        "incompatible_flags",
        "index_locked",
        "branch_already_exists",
        "branch_not_found",
        "no_previous_branch",
        "branch_invalid_name",
        "branch_not_fully_merged",
        "branch_checked_out",
        "branch_no_commits",
        "branch_invalid_start_point",
        "branch_upstream_not_tracking",
        "branch_no_upstream",
        "branch_error",
        "hook_failed",
        "revision_not_found",
    ] {
        assert!(
            text.contains(&format!("`{slug}`")),
            "exit-codes.md must document slug `{slug}`"
        );
    }
}

#[test]
fn exit_status_for_every_error_kind_cli() {
    let root = init_repo();
    let cases: &[(&str, &[&str], &str, i32)] = &[
        ("usage", &["branch", "--delete", "--json"], "usage", 2),
        (
            "incompatible_flags",
            &["log", "--graph", "--json", "-n", "1"],
            "incompatible_flags",
            1,
        ),
        (
            "invalid_pattern",
            &["log", "--author", "[", "--json", "-n", "1"],
            "invalid_pattern",
            129,
        ),
        (
            "branch_not_found",
            &["branch", "-d", "missing", "--json"],
            "branch_not_found",
            1,
        ),
        (
            "branch_invalid_name",
            &["branch", "bad..name", "--json"],
            "branch_invalid_name",
            1,
        ),
    ];

    for (name, args, kind, code) in cases {
        let (got_code, stdout) = grit(root.path(), args);
        assert_eq!(got_code, *code, "{name}");
        let v: Value = serde_json::from_str(stdout.trim()).expect(name);
        assert_eq!(v["kind"].as_str(), Some(*kind), "{name}");
    }

    fs::write(root.path().join("a.txt"), "a").expect("write");
    let (code, _) = grit(root.path(), &["commit", "-m", "seed", "--json"]);
    assert_eq!(code, 0);
    let _ = grit(root.path(), &["branch", "dup"]);
    let (code, stdout) = grit(root.path(), &["branch", "dup", "--json"]);
    assert_ne!(code, 0);
    let v: Value = serde_json::from_str(stdout.trim()).expect("branch_already_exists");
    assert_eq!(v["kind"].as_str(), Some("branch_already_exists"));

    fs::write(root.path().join("a.txt"), "b").expect("write");
    let lock = root.path().join(".git/index.lock");
    fs::write(&lock, "").expect("foreign lock");
    let (code, stdout) = grit(root.path(), &["commit", "-m", "x", "--json", "--", "a.txt"]);
    fs::remove_file(&lock).expect("remove lock");
    assert_eq!(code, 1, "index_locked");
    let v: Value = serde_json::from_str(stdout.trim()).expect("index_locked");
    assert_eq!(v["kind"].as_str(), Some("index_locked"));

    let (code, stdout) = grit(root.path(), &["merge", "nonexistent", "--json"]);
    assert_eq!(code, 1, "revision_not_found");
    let v: Value = serde_json::from_str(stdout.trim()).expect("revision_not_found");
    assert_eq!(v["kind"].as_str(), Some("revision_not_found"));
}

/// Port with nothing listening (bound, then released).
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.local_addr().expect("addr").port()
}

#[test]
fn unreadable_remote_exits_128_for_every_transfer_command() {
    let root = init_repo();
    fs::write(root.path().join("a.txt"), "a").expect("write");
    let (code, _) = grit(root.path(), &["commit", "-m", "seed", "--json"]);
    assert_eq!(code, 0);

    let scratch = tempfile::tempdir().expect("tempdir");
    let missing = scratch.path().join("missing.git");
    let missing = missing.to_str().expect("utf8").to_owned();
    let refused = format!("http://127.0.0.1:{}/x.git", closed_port());
    let (code, _) = grit(root.path(), &["remote", "add", "origin", &missing]);
    assert_eq!(code, 0);

    for (label, url) in [("missing path", &missing), ("refused port", &refused)] {
        let (code, _) = grit(root.path(), &["remote", "set-url", "origin", url]);
        assert_eq!(code, 0, "{label}: set-url");
        let dest = scratch.path().join("dest");
        let dest = dest.to_str().expect("utf8");
        let cases: [&[&str]; 6] = [
            &["clone", url, dest, "--json"],
            &["fetch", "--json"],
            &["pull", "--json"],
            &["push", "--json"],
            &["remote", "refs", "origin", "--json"],
            &["remote", "refs", url, "--json"],
        ];
        for args in cases {
            let (code, stdout) = grit(root.path(), args);
            assert_eq!(code, 128, "{label}: {args:?} -> {stdout}");
            let v: Value = serde_json::from_str(stdout.trim()).expect("json error");
            assert_eq!(
                v["kind"].as_str(),
                Some("remote_unreadable"),
                "{label}: {args:?}"
            );
            assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()));
        }
        assert!(
            !scratch.path().join("dest").exists(),
            "{label}: failed clone must not leave the destination"
        );
    }

    let (code, out) = grit(root.path(), &["fetch"]);
    assert_eq!(code, 128, "human output keeps the exit code: {out}");
}

/// git:// peer that reads the request and hangs up without an advertisement.
fn silent_git_daemon() -> u16 {
    use std::io::Read;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
        }
    });
    port
}

#[test]
fn non_repository_remotes_exit_128() {
    let root = init_repo();
    fs::write(root.path().join("a.txt"), "a").expect("write");
    let (code, _) = grit(root.path(), &["commit", "-m", "seed", "--json"]);
    assert_eq!(code, 0);

    let scratch = tempfile::tempdir().expect("tempdir");
    let objects_only = scratch.path().join("fake.git");
    fs::create_dir_all(objects_only.join("objects")).expect("mkdir");
    let objects_only = objects_only.to_str().expect("utf8").to_owned();
    let silent = format!("git://127.0.0.1:{}/repo.git", silent_git_daemon());
    let (code, _) = grit(root.path(), &["remote", "add", "origin", &objects_only]);
    assert_eq!(code, 0);

    for (label, url) in [
        ("objects-only dir", &objects_only),
        ("silent git://", &silent),
    ] {
        let (code, _) = grit(root.path(), &["remote", "set-url", "origin", url]);
        assert_eq!(code, 0, "{label}: set-url");
        let cases: [&[&str]; 3] = [
            &["remote", "refs", url, "--json"],
            &["fetch", "--json"],
            &["push", "--json"],
        ];
        for args in cases {
            let (code, stdout) = grit(root.path(), args);
            assert_eq!(code, 128, "{label}: {args:?} -> {stdout}");
            let v: Value = serde_json::from_str(stdout.trim()).expect("json error");
            assert_eq!(
                v["kind"].as_str(),
                Some("remote_unreadable"),
                "{label}: {args:?}"
            );
        }
    }
}
