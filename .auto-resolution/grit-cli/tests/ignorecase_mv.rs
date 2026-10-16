//! Case-only renames via `grit mv` with `core.ignorecase=true` (issue #968).

use std::fs;
use std::path::Path;
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

fn worktree_has_exact_dir_entry(parent: &Path, name: &str) -> bool {
    fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy() == name)
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
fn grit_mv_case_only_rename_updates_index() {
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

    fs::write(dir.path().join("README.md"), b"hi\n").expect("write");
    assert!(run_grit(grit, dir.path(), &["add", "README.md"], &null)
        .status
        .success());
    assert!(
        run_grit(grit, dir.path(), &["commit", "-m", "first"], &null)
            .status
            .success()
    );

    let mv = run_grit(grit, dir.path(), &["mv", "README.md", "readme.md"], &null);
    assert!(
        mv.status.success(),
        "grit mv failed: {}",
        String::from_utf8_lossy(&mv.stderr)
    );
    assert!(worktree_has_exact_dir_entry(dir.path(), "readme.md"));
    assert!(!worktree_has_exact_dir_entry(dir.path(), "README.md"));

    let index = Index::load(&git_dir.join("index")).expect("index");
    let stage0: Vec<_> = index.entries().iter().filter(|e| e.stage() == 0).collect();
    assert_eq!(stage0.len(), 1);
    assert_eq!(String::from_utf8_lossy(&stage0[0].path), "readme.md");

    if Command::new("git").output().is_ok() {
        let ls = run_git(dir.path(), &["ls-files"]);
        assert!(ls.status.success());
        assert_eq!(String::from_utf8_lossy(&ls.stdout).trim(), "readme.md");
        let diff = run_git(dir.path(), &["diff", "--cached", "-M"]);
        assert!(diff.status.success());
        let diff_out = String::from_utf8_lossy(&diff.stdout);
        assert!(
            diff_out.contains("rename from") || diff_out.contains("similarity index"),
            "git should see a staged rename: {diff_out}"
        );
    }
}
