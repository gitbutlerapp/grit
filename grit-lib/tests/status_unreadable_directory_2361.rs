//! Unreadable directories must warn and must not abort status or commit (issue #2361).

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::diagnostics::{CollectingDiagnostics, DiagnosticsHandle, Warning};
    use grit_lib::porcelain::commit::{commit_worktree, CommitRequest};
    use grit_lib::porcelain::status::{status, StatusOptions};
    use grit_lib::progress::{NeverCancel, NullProgress};
    use grit_lib::repo::Repository;
    use grit_test_support::git;

    fn set_mode(path: &Path, mode: u32) {
        let mut perms = fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms).expect("set_permissions");
    }

    struct RepoFixture {
        repo: Repository,
        diagnostics: Arc<CollectingDiagnostics>,
    }

    fn open_repo(root: &Path) -> RepoFixture {
        let diagnostics = Arc::new(CollectingDiagnostics::new());
        let mut repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("open repo");
        repo.set_diagnostics(Arc::clone(&diagnostics) as DiagnosticsHandle);
        RepoFixture { repo, diagnostics }
    }

    fn init_base_commit(root: &Path) {
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("a"), "a\n").expect("write a");
        fs::create_dir(root.join("d")).expect("mkdir d");
        fs::write(root.join("d/c"), "c\n").expect("write d/c");
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
    }

    #[test]
    fn unreadable_parent_of_tracked_file_warns_and_status_continues() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_base_commit(root);

        set_mode(&root.join("d"), 0o000);
        if fs::read_dir(root.join("d")).is_ok() {
            set_mode(&root.join("d"), 0o755);
            return;
        }

        fs::write(root.join("a"), "A\n").expect("edit sibling");

        let fx = open_repo(root);
        let model = status(
            &fx.repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status must succeed");
        assert!(
            model.unstaged.iter().any(|e| e.path() == "a"),
            "sibling edit must appear: {:?}",
            model.unstaged
        );
        assert!(
            !model.unstaged.iter().any(|e| e.path() == "d/c"),
            "tracked file under unreadable dir must not be listed modified/deleted"
        );

        let warnings = fx.diagnostics.warnings();
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                Warning::WorktreeAccessFailed { path, .. }
                    if path.contains("d/c") || path.starts_with("d/")
            )),
            "expected warning naming d/ or d/c, got {warnings:?}"
        );

        let git_status = git(root, &["status", "--porcelain"]);
        assert_eq!(git_status.trim(), " M a");
        assert_eq!(git(root, &["status"]).lines().next().unwrap(), "");

        set_mode(&root.join("d"), 0o755);
    }

    #[test]
    fn unreadable_untracked_directory_warns_without_aborting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_base_commit(root);
        set_mode(&root.join("d"), 0o755);

        fs::create_dir(root.join("ud")).expect("mkdir ud");
        fs::write(root.join("ud/f"), "x\n").expect("write ud/f");
        set_mode(&root.join("ud"), 0o000);
        if fs::read_dir(root.join("ud")).is_ok() {
            set_mode(&root.join("ud"), 0o755);
            return;
        }

        let fx = open_repo(root);
        let model = status(
            &fx.repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status must succeed");
        assert!(
            model.is_clean(),
            "tree should look clean aside from warnings"
        );

        let warnings = fx.diagnostics.warnings();
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                Warning::WorktreeAccessFailed { path, .. } if path.starts_with("ud")
            )),
            "expected warning for ud/, got {warnings:?}"
        );

        set_mode(&root.join("ud"), 0o755);
    }

    #[test]
    fn commit_other_change_keeps_tracked_file_under_unreadable_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_base_commit(root);

        set_mode(&root.join("d"), 0o000);
        if fs::read_dir(root.join("d")).is_ok() {
            set_mode(&root.join("d"), 0o755);
            return;
        }

        fs::write(root.join("a"), "A\n").expect("edit a");

        let fx = open_repo(root);
        let req = CommitRequest::new("second\n");
        commit_worktree(&fx.repo, &req, &mut NullProgress, &NeverCancel)
            .expect("commit must succeed");

        set_mode(&root.join("d"), 0o755);
        let tree = git(root, &["ls-tree", "-r", "--name-only", "HEAD"]);
        assert!(tree.lines().any(|l| l == "d/c"), "d/c must remain tracked");
        git(root, &["fsck"]);
    }
}
