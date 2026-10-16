//! Worktree↔index diffs normalize `eol=crlf` worktree bytes before line diffs (issue #990).

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    diff_index_to_worktree_with_options, read_worktree_side_bytes_for_diff,
    DiffIndexToWorktreeOptions, DiffStatus,
};
use grit_lib::repo::Repository;
use grit_lib::worktree::worktree_rules::WorktreeRules;
use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn init_git_repo(root: &TempDir) -> std::path::PathBuf {
    let repo = root.path().join("rp");
    fs::create_dir_all(&repo).expect("mkdir");
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .expect("git init")
        .success());
    assert!(Command::new("git")
        .args(["config", "user.name", "a"])
        .current_dir(&repo)
        .status()
        .expect("user.name")
        .success());
    assert!(Command::new("git")
        .args(["config", "user.email", "a@b"])
        .current_dir(&repo)
        .status()
        .expect("user.email")
        .success());
    repo
}

#[test]
fn diff_index_to_worktree_eol_crlf_single_line_change() {
    let root = TempDir::new().expect("tempdir");
    let repo = init_git_repo(&root);
    fs::write(repo.join(".gitattributes"), "*.txt text eol=crlf\n").expect("attrs");
    fs::write(repo.join("f.txt"), "a\nb\nc\n").expect("blob lf");
    assert!(Command::new("git")
        .args(["add", "-A"])
        .current_dir(&repo)
        .status()
        .expect("add")
        .success());
    assert!(Command::new("git")
        .args(["commit", "-qm", "1"])
        .current_dir(&repo)
        .status()
        .expect("commit")
        .success());

    fs::remove_file(repo.join("f.txt")).expect("rm");
    assert!(Command::new("git")
        .args(["checkout", "--", "f.txt"])
        .current_dir(&repo)
        .status()
        .expect("checkout")
        .success());
    fs::write(repo.join("f.txt"), "a\r\nB\r\nc\r\n").expect("crlf edit");

    let git_stat = Command::new("git")
        .args(["diff", "--stat"])
        .current_dir(&repo)
        .output()
        .expect("git diff");
    assert!(git_stat.status.success());
    let stat = String::from_utf8_lossy(&git_stat.stdout);
    assert!(
        stat.contains("1 insertion") && stat.contains("1 deletion"),
        "git should report one line changed: {stat}"
    );

    let grit_repo = Repository::open(
        &repo.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&repo),
    )
    .expect("open");
    let work_tree = grit_repo.work_tree().expect("wt");
    let mut index = grit_repo.index_handle().load().expect("index");
    let rules = WorktreeRules::from_repository(&grit_repo, &index).expect("rules");
    let cfg =
        ConfigSet::load(&Environment::empty(), Some(grit_repo.git_dir()), true).expect("config");
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(
        WorktreeRules::from_repository(&grit_repo, &index).expect("rules arc"),
    ));
    let opts = DiffIndexToWorktreeOptions {
        repository_git_dir: Some(grit_repo.git_dir().to_path_buf()),
        config: Some(rules.config_arc()),
        worktree_rules: Some(rules_arc),
        ..DiffIndexToWorktreeOptions::default()
    };
    let (entries, _) =
        diff_index_to_worktree_with_options(grit_repo.odb(), &mut index, work_tree, opts)
            .expect("diff");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, DiffStatus::Modified);

    let old = grit_repo
        .odb()
        .read(&entries[0].old_oid)
        .expect("old blob")
        .data;
    let new = read_worktree_side_bytes_for_diff(
        grit_repo.odb(),
        work_tree,
        "f.txt",
        u32::from_str_radix(&entries[0].new_mode, 8).expect("mode"),
        &cfg,
        index.get(b"f.txt", 0),
        Some(&rules),
    )
    .expect("new side");
    assert_eq!(old, b"a\nb\nc\n");
    assert_eq!(new, b"a\nB\nc\n");

    let old_lines = old.split(|&b| b == b'\n').collect::<Vec<_>>();
    let new_lines = new.split(|&b| b == b'\n').collect::<Vec<_>>();
    let mut changes = 0usize;
    for (o, n) in old_lines.iter().zip(new_lines.iter()) {
        if o != n {
            changes += 1;
        }
    }
    assert_eq!(changes, 1, "normalized sides differ on one line only");
}
