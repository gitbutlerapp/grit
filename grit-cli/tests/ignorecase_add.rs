//! `core.ignorecase=true`: case-only renames (issue #916) and case-insensitive ignore rules
//! (issue #1912).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::process::Command;

use grit_lib::index::Index;

fn null_config() -> String {
    if cfg!(windows) {
        "NUL".to_string()
    } else {
        "/dev/null".to_string()
    }
}

fn run_grit(grit: &str, dir: &std::path::Path, args: &[&str], null: &str) -> std::process::Output {
    Command::new(grit)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_CONFIG_SYSTEM", null)
        .output()
        .expect("run grit")
}

fn run_git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run git")
}

#[test]
fn grit_add_replaces_case_variant_without_duplicate_index_entries() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let null = null_config();
    let git_dir = dir.path().join(".git");

    assert!(run_grit(grit, dir.path(), &["init"], &null)
        .status
        .success());
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tignorecase = true\n\tbare = false\n[user]\n\tname = Test\n\temail = t@example.com\n",
    )
    .expect("config");

    fs::write(dir.path().join("File.txt"), b"x\n").expect("write");
    assert!(run_grit(grit, dir.path(), &["add", "File.txt"], &null)
        .status
        .success());
    assert!(
        run_grit(grit, dir.path(), &["commit", "-m", "first"], &null)
            .status
            .success()
    );

    fs::rename(dir.path().join("File.txt"), dir.path().join("file.txt")).expect("rename");
    assert!(run_grit(grit, dir.path(), &["add"], &null).status.success());
    assert!(run_grit(grit, dir.path(), &["commit", "-m", "case"], &null)
        .status
        .success());

    let index = Index::load(&git_dir.join("index")).expect("index");
    let stage0: Vec<_> = index.entries().iter().filter(|e| e.stage() == 0).collect();
    assert_eq!(stage0.len(), 1, "expected a single tracked file");
    assert_eq!(
        String::from_utf8_lossy(&stage0[0].path),
        "file.txt",
        "index must use the on-disk spelling after case-only rename"
    );

    if Command::new("git").output().is_ok() {
        let ls = run_git(dir.path(), &["ls-files"]);
        assert!(ls.status.success(), "git ls-files failed");
        let listed = String::from_utf8_lossy(&ls.stdout);
        assert_eq!(
            listed.trim(),
            "file.txt",
            "system git must see one path, got: {listed}"
        );
        let st = run_git(dir.path(), &["status", "--short"]);
        assert!(st.status.success());
        assert!(
            String::from_utf8_lossy(&st.stdout).trim().is_empty(),
            "git status should be clean after case-only add/commit"
        );
    }
}

/// Issue #1912: with `core.ignorecase=true`, `Build/` and `*.LOG` ignore `build/` and `debug.log`.
#[test]
fn ignore_check_status_and_add_honor_core_ignorecase() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let null = null_config();
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"], &null).status.success());
    fs::write(
        root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tignorecase = true\n\tbare = false\n[user]\n\tname = Test\n\temail = t@example.com\n",
    )
    .expect("config");
    fs::write(root.join(".gitignore"), "Build/\n*.LOG\n!keep.log\n").expect("gitignore");
    fs::create_dir_all(root.join("build")).expect("build");
    for file in ["build/x.o", "debug.log", "keep.log", "README"] {
        fs::write(root.join(file), "x\n").expect("file");
    }

    let check = run_grit(
        grit,
        root,
        &[
            "ignore",
            "check",
            "--json",
            "--all",
            "build/x.o",
            "debug.log",
            "keep.log",
        ],
        &null,
    );
    assert_eq!(check.status.code(), Some(0), "{check:?}");
    let json: serde_json::Value = serde_json::from_slice(&check.stdout).expect("json");
    assert_eq!(json["any_ignored"], true);
    let ignored: Vec<(&str, bool, &str)> = json["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap_or_default(),
                e["ignored"].as_bool().unwrap_or_default(),
                e["matched"]["pattern_text"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        ignored,
        [
            ("build/x.o", true, "Build/"),
            ("debug.log", true, "*.LOG"),
            ("keep.log", false, "!keep.log"),
        ]
    );

    let status = run_grit(
        grit,
        root,
        &["status", "--json", "--untracked", "all"],
        &null,
    );
    assert!(status.status.success(), "{status:?}");
    let json: serde_json::Value = serde_json::from_slice(&status.stdout).expect("json");
    let mut untracked: Vec<&str> = json["untracked"]
        .as_array()
        .expect("untracked")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    untracked.sort_unstable();
    assert_eq!(untracked, [".gitignore", "README", "keep.log"]);

    assert!(run_grit(grit, root, &["add"], &null).status.success());
    let index = Index::load(&root.join(".git/index")).expect("index");
    let mut staged: Vec<String> = index
        .entries()
        .iter()
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    staged.sort_unstable();
    assert_eq!(staged, [".gitignore", "README", "keep.log"]);

    if Command::new("git").output().is_ok() {
        let ls = run_git(root, &["ls-files", "-o", "--exclude-standard"]);
        assert!(ls.status.success(), "git ls-files failed");
        assert!(ls.stdout.is_empty(), "git sees unstaged non-ignored files");
    }
}
