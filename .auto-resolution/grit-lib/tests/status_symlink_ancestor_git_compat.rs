//! `core.symlinks=false` must not disable ancestor symlink detection (issue #1231 review).
//!
//! When a tracked directory is replaced by a symlink, indexed paths under it are deletions
//! even if the link target holds a file at the same relative path with identical content.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::porcelain::status::{status, StatusOptions};
    use grit_lib::progress::NullProgress;
    use grit_lib::repo::Repository;
    use grit_test_support::git;

    #[test]
    fn directory_replaced_by_symlink_reports_deletion_with_core_symlinks_false() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "core.symlinks", "false"]);

        fs::create_dir_all(root.join("d")).expect("mkdir d");
        fs::write(root.join("d/f"), b"same payload\n").expect("write tracked");
        git(root, &["add", "d/f"]);
        git(root, &["commit", "-qm", "track d/f"]);

        fs::remove_dir_all(root.join("d")).expect("remove d");
        let alt = root.join("alt");
        fs::create_dir_all(&alt).expect("mkdir alt");
        fs::write(alt.join("f"), b"same payload\n").expect("write alt copy");
        symlink(&alt, root.join("d")).expect("symlink d -> alt");

        let git_status = git(root, &["status", "--porcelain"]);
        assert!(
            git_status.lines().any(|line| line.starts_with(" D d/f")),
            "system git must report d/f deleted, got:\n{git_status}"
        );
        let git_diff = git(root, &["diff", "--name-only"]);
        assert!(
            git_diff.lines().any(|line| line == "d/f"),
            "system git diff must list d/f, got:\n{git_diff}"
        );

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("open grit repo");
        let model = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        assert!(
            model
                .unstaged
                .iter()
                .any(|e| e.path() == "d/f" && e.status == grit_lib::diff::DiffStatus::Deleted),
            "grit status must report d/f deleted like git, got unstaged {:?} untracked {:?}",
            model.unstaged,
            model.untracked
        );

        let grit = std::env::var("CARGO_BIN_EXE_grit")
            .ok()
            .map(std::path::PathBuf::from);
        if let Some(grit) = grit.filter(|p| p.is_file()) {
            let out = Command::new(&grit)
                .args(["diff", "--json"])
                .current_dir(root)
                .output()
                .expect("grit diff --json");
            assert!(
                out.status.success(),
                "grit diff failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
            let paths: Vec<_> = v
                .get("files")
                .and_then(|f| f.as_array())
                .into_iter()
                .flatten()
                .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
                .collect();
            assert!(
                paths.iter().any(|p| *p == "d/f"),
                "grit diff must list d/f, got {paths:?}"
            );
        }
    }
}
