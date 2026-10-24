//! FIFOs, unix sockets, and other special nodes are ignored like Git (issue #1400).

#[cfg(unix)]
mod unix {
    use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
    use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
    use grit_lib::porcelain::status::{status, StatusOptions};
    use grit_lib::progress::NullProgress;
    use grit_lib::repo::Repository;
    use grit_test_support::git;
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::process::Command;

    fn git_untracked_lines(root: &Path) -> Vec<String> {
        let out = git(root, &["status", "--porcelain", "-u"]);
        out.lines()
            .filter(|l| l.starts_with("??"))
            .map(|l| l[3..].trim().to_owned())
            .collect()
    }

    fn grit_untracked(root: &Path) -> Vec<String> {
        let repo = Repository::discover(Some(root), &Default::default()).expect("open");
        let model = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        model.untracked.clone()
    }

    fn ident() -> String {
        "Special Files <special@example.com> 1700000000 +0000".to_owned()
    }

    fn commit_req(message: &str) -> CommitRequest {
        let id = ident();
        CommitRequest {
            message: message.to_owned(),
            author: Some(id.parse().expect("signature")),
            committer: Some(id.parse().expect("signature")),
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            ..Default::default()
        }
    }

    fn init_repo_with_base(root: &Path) {
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.email", "special@example.com"]);
        git(root, &["config", "user.name", "Special Files"]);
        fs::write(root.join("ok.txt"), "ok\n").expect("write ok");
        git(root, &["add", "ok.txt"]);
        git(root, &["commit", "-qm", "base"]);
    }

    #[test]
    fn fifo_and_socket_untracked_match_git() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        init_repo_with_base(root);

        let fifo = root.join("pipe");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo")
                .success(),
            "mkfifo failed"
        );
        let sock = root.join("sock");
        let _listener = UnixListener::bind(&sock).expect("bind unix socket");
        fs::write(root.join("new.txt"), "new\n").expect("write new");

        let git_u = git_untracked_lines(root);
        let grit_u = grit_untracked(root);
        assert_eq!(
            git_u, grit_u,
            "untracked lists must match git (git {git_u:?}, grit {grit_u:?})"
        );
        assert!(
            git_u == ["new.txt"],
            "only regular file is untracked, got git {git_u:?}"
        );
    }

    #[test]
    fn add_with_fifo_stages_regular_file_only() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        init_repo_with_base(root);

        assert!(Command::new("mkfifo")
            .arg(root.join("pipe"))
            .status()
            .expect("mkfifo")
            .success());
        fs::write(root.join("new.txt"), "new\n").expect("write new");

        let repo = Repository::discover(Some(root), &Default::default()).expect("open");
        let outcome = stage(
            &repo,
            &StageOptions {
                mode: StageMode::All,
                ..StageOptions::default()
            },
            &mut NullProgress,
        )
        .expect("stage must not hang or fail on fifo");
        assert_eq!(
            outcome.added.len(),
            1,
            "only new.txt staged, got {outcome:?}"
        );
        assert_eq!(outcome.added[0], "new.txt");

        let staged = git(root, &["diff", "--cached", "--name-only"]);
        assert_eq!(staged.trim(), "new.txt");
        let git_status = git(root, &["status", "--porcelain"]);
        assert!(
            !git_status.contains("pipe"),
            "fifo must not appear in status after add"
        );
    }

    #[test]
    fn commit_auto_stage_with_unix_socket_succeeds() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        init_repo_with_base(root);

        let sock = root.join("sock");
        let _listener = UnixListener::bind(&sock).expect("bind unix socket");
        fs::write(root.join("new.txt"), "new\n").expect("write new");

        let repo = Repository::discover(Some(root), &Default::default()).expect("open");
        commit_with_scope(
            &repo,
            &commit_req("second"),
            CommitScope::AutoStage,
            &mut NullProgress,
        )
        .expect("auto-stage commit with socket present");
        git(root, &["fsck"]);
        let log = git(root, &["log", "--oneline", "-2"]);
        assert!(
            log.lines().count() >= 2,
            "commit must succeed with socket in tree"
        );
    }
}
