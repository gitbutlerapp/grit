//! `grit gc`, `repack`, and `fsck` are registered subcommands.

use std::process::Command;

#[test]
fn maintenance_subcommands_help() {
    for cmd in ["gc", "repack", "fsck", "verify"] {
        let out = Command::new(env!("CARGO_BIN_EXE_grit"))
            .arg(cmd)
            .arg("--help")
            .output()
            .unwrap_or_else(|e| panic!("spawn grit {cmd}: {e}"));
        assert!(
            out.status.success(),
            "grit {cmd} --help: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository whose only pack stores every object whole.
fn repo_without_deltas() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let mut body = String::new();
    for i in 0..30 {
        for line in 0..20 {
            body.push_str(&format!(
                "line {line} of version {i} with shared padding text\n"
            ));
        }
        std::fs::write(dir.path().join("notes.txt"), &body)
            .unwrap_or_else(|e| panic!("write: {e}"));
        git(dir.path(), &["add", "notes.txt"]);
        git(dir.path(), &["commit", "-qm", &format!("v{i}")]);
    }
    git(dir.path(), &["repack", "-adfq", "--window=0"]);
    dir
}

fn grit(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn grit {args:?}: {e}"));
    assert!(
        out.status.success(),
        "grit {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn delta_count(dir: &std::path::Path) -> usize {
    let pack_dir = dir.join(".git/objects/pack");
    let idx = std::fs::read_dir(&pack_dir)
        .unwrap_or_else(|e| panic!("read pack dir: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .unwrap_or_else(|| panic!("no pack index"));
    let listing = git(dir, &["verify-pack", "-v", &idx.to_string_lossy()]);
    listing
        .lines()
        .filter(|line| line.split_whitespace().count() >= 7)
        .count()
}

#[test]
fn repack_force_recomputes_deltas_and_reports_json() {
    let dir = repo_without_deltas();
    assert_eq!(delta_count(dir.path()), 0);

    let out = grit(dir.path(), &["repack", "--force", "--json"]);
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("json: {e}"));
    assert_eq!(json["dry_run"], false);
    assert_eq!(json["packs_before"], 1);
    assert!(json["objects_packed"].as_u64().is_some_and(|n| n >= 90));
    assert!(json["pack_path"].is_string());

    git(dir.path(), &["fsck", "--strict", "--no-dangling"]);
    assert!(delta_count(dir.path()) > 0, "--force should find deltas");
}

#[test]
fn gc_repack_force_markdown_reports_the_task() {
    let dir = repo_without_deltas();
    let out = grit(dir.path(), &["gc", "repack", "--force", "--markdown"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("repack"), "{text}");
    git(dir.path(), &["fsck", "--strict", "--no-dangling"]);
    assert!(delta_count(dir.path()) > 0);
}
