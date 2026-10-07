//! Case-only renames with `core.ignorecase=true` (issue #916).

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
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tignorecase = true\n\tbare = false\n",
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
