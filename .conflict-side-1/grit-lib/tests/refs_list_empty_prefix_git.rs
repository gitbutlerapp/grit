//! Empty-prefix ref listing matches system `git for-each-ref` (no pseudoref scan).

use std::fs;
use std::process::Command;

use grit_lib::refs::list_refs;
use tempfile::tempdir;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn hermetic_git(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
}

#[test]
fn empty_prefix_list_refs_matches_git_for_each_ref_with_resolvable_head() {
    if !git_available() {
        return;
    }
    let dir = tempdir().expect("tempdir");
    let git_dir = dir.path();
    let mut init_cmd = Command::new("git");
    init_cmd.args(["init"]).current_dir(git_dir);
    let init = hermetic_git(&mut init_cmd).status().expect("git init");
    assert!(init.success());

    let mut commit_cmd = Command::new("git");
    commit_cmd
        .args(["commit", "--allow-empty", "-m", "seed"])
        .current_dir(git_dir);
    let oid = hermetic_git(&mut commit_cmd).output().expect("git commit");
    assert!(oid.status.success(), "git commit failed");

    let dot_git = git_dir.join(".git");
    let store = if dot_git.is_dir() {
        dot_git
    } else {
        git_dir.to_path_buf()
    };

    let grit_names: Vec<String> = list_refs(&store, "")
        .expect("list_refs")
        .into_iter()
        .map(|(name, _)| name)
        .collect();

    let git_out = Command::new("git")
        .args(["for-each-ref"])
        .current_dir(git_dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git for-each-ref");
    assert!(
        git_out.status.success(),
        "git for-each-ref failed: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );
    let git_names: Vec<String> = String::from_utf8_lossy(&git_out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .map(str::to_owned)
        .collect();

    assert_eq!(grit_names, git_names);
    assert!(
        !grit_names.iter().any(|n| n == "HEAD"),
        "empty-prefix listing must not include HEAD: {grit_names:?}"
    );
    assert!(
        grit_names
            .iter()
            .any(|n| n == "refs/heads/main" || n.starts_with("refs/heads/")),
        "expected at least one branch ref: {grit_names:?}"
    );
}
