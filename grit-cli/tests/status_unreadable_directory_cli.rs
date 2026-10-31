//! Issue #2361: unreadable directories warn and status/commit continue.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    use grit_test_support::configure_git_env;

    const GRIT: &str = env!("CARGO_BIN_EXE_grit");

    fn grit_cmd() -> Command {
        let mut cmd = Command::new(GRIT);
        configure_git_env(&mut cmd);
        cmd
    }

    fn git_cmd() -> Command {
        let mut cmd = Command::new("git");
        configure_git_env(&mut cmd);
        cmd
    }

    fn set_mode(path: &Path, mode: u32) {
        let mut perms = fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms).expect("set_permissions");
    }

    fn init_repo(root: &Path) {
        let out = grit_cmd()
            .args(["init", "."])
            .current_dir(root)
            .output()
            .expect("grit init");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        git_cmd()
            .args(["config", "user.email", "t@example.com"])
            .current_dir(root)
            .status()
            .expect("git config email");
        git_cmd()
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .status()
            .expect("git config name");
        fs::write(root.join("a"), "a\n").expect("write a");
        fs::create_dir(root.join("d")).expect("mkdir d");
        fs::write(root.join("d/c"), "c\n").expect("write d/c");
        let out = grit_cmd()
            .args(["commit", "-m", "base"])
            .current_dir(root)
            .output()
            .expect("commit base");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn status_and_commit_survive_unreadable_tracked_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_repo(root);

        set_mode(&root.join("d"), 0o000);
        if fs::read_dir(root.join("d")).is_ok() {
            set_mode(&root.join("d"), 0o755);
            return;
        }

        fs::write(root.join("a"), "A\n").expect("edit a");

        let status = grit_cmd()
            .args(["status", "--json"])
            .current_dir(root)
            .output()
            .expect("status json");
        assert!(
            status.status.success(),
            "status failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );
        let stderr = String::from_utf8_lossy(&status.stderr);
        assert!(
            stderr.contains("d/c") || stderr.contains("d/"),
            "stderr should name path: {stderr}"
        );
        let v: serde_json::Value = serde_json::from_slice(&status.stdout).expect("json");
        assert!(v
            .get("warnings")
            .and_then(|w| w.as_array())
            .is_some_and(|a| !a.is_empty()));
        assert_eq!(v["unstaged"].as_array().map(|a| a.len()), Some(1), "{v}");

        let commit = grit_cmd()
            .args(["commit", "-m", "second"])
            .current_dir(root)
            .output()
            .expect("commit");
        assert!(
            commit.status.success(),
            "commit failed:\n{}",
            String::from_utf8_lossy(&commit.stderr)
        );

        set_mode(&root.join("d"), 0o755);
        let ls_tree = git_cmd()
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .current_dir(root)
            .output()
            .expect("ls-tree");
        let tree = String::from_utf8_lossy(&ls_tree.stdout);
        assert!(tree.lines().any(|l| l == "d/c"));
    }

    #[test]
    fn status_warns_on_unreadable_untracked_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_repo(root);
        set_mode(&root.join("d"), 0o755);

        fs::create_dir(root.join("ud")).expect("mkdir ud");
        fs::write(root.join("ud/f"), "x\n").expect("write ud/f");
        set_mode(&root.join("ud"), 0o000);
        if fs::read_dir(root.join("ud")).is_ok() {
            set_mode(&root.join("ud"), 0o755);
            return;
        }

        let out = grit_cmd()
            .args(["status"])
            .current_dir(root)
            .output()
            .expect("status");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("ud/") || stderr.contains("ud"),
            "expected warning for ud/: {stderr}"
        );

        set_mode(&root.join("ud"), 0o755);
    }
}
