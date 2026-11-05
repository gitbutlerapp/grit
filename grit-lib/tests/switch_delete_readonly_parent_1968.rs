//! Switch must fail when a checkout deletion cannot remove a file (issue #1968).

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::error::{Error, WorktreePathOperation};
    use grit_lib::objects::ObjectId;
    use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
    use grit_lib::repo::Repository;
    use grit_test_support::git;

    fn init_repo(root: &Path) {
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
    }

    fn assert_clean_worktree(root: &Path) {
        let porcelain = git(root, &["status", "--porcelain"]);
        assert!(
            porcelain.trim().is_empty(),
            "worktree must match index/HEAD, got:\n{porcelain}"
        );
    }

    fn chmod_dir_read_only(root: &Path, dir: &str) {
        let path = root.join(dir);
        let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(path, perms).expect("chmod");
    }

    fn chmod_dir_writable(root: &Path, dir: &str) {
        let path = root.join(dir);
        let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod");
    }

    fn switch_to_del(repo: &Repository, del_oid: ObjectId) -> Error {
        switch_repository(
            repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "del".into(),
                    oid: del_oid,
                },
                dry_run: false,
                hooks: grit_lib::command_runner::hooks::PostOperationHooks::Skip,
            },
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect_err("switch must fail when a deletion cannot be removed")
    }

    #[test]
    fn switch_fails_when_deleted_path_cannot_be_unlinked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        init_repo(tmp.path());

        std::fs::create_dir_all(tmp.path().join("d")).expect("mkdir d");
        std::fs::write(tmp.path().join("d/a"), "a\n").expect("write d/a");
        std::fs::write(tmp.path().join("b"), "b\n").expect("write b");
        git(tmp.path(), &["add", "-A"]);
        git(tmp.path(), &["commit", "-qm", "base"]);

        git(tmp.path(), &["switch", "-q", "-c", "del"]);
        git(tmp.path(), &["rm", "-q", "d/a"]);
        git(tmp.path(), &["commit", "-qm", "del"]);
        let del_oid =
            ObjectId::from_hex(git(tmp.path(), &["rev-parse", "del"]).trim()).expect("del oid");

        git(tmp.path(), &["switch", "-q", "main"]);
        let main_head = git(tmp.path(), &["rev-parse", "HEAD"]);

        chmod_dir_read_only(tmp.path(), "d");

        let repo = Repository::open(
            &tmp.path().join(".git"),
            &RepositoryOptions::empty().with_work_tree(tmp.path()),
        )
        .expect("open grit repo");

        let err = switch_to_del(&repo, del_oid);

        let msg = err.to_string();
        assert!(msg.contains("d/a"), "error must name the path, got: {msg}");
        assert!(
            msg.contains("remove"),
            "error must describe removal, got: {msg}"
        );
        match &err {
            Error::WorktreePathIo {
                path,
                operation: WorktreePathOperation::Remove,
                ..
            } => assert_eq!(path, "d/a"),
            other => panic!("unexpected error: {other:?}"),
        }

        assert_eq!(git(tmp.path(), &["rev-parse", "HEAD"]), main_head);
        assert!(tmp.path().join("d/a").is_file());
        assert_clean_worktree(tmp.path());

        chmod_dir_writable(tmp.path(), "d");
    }

    #[test]
    fn switch_rollback_restores_earlier_deletions_on_later_unlink_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        init_repo(tmp.path());

        std::fs::write(tmp.path().join("a"), "a\n").expect("write a");
        std::fs::create_dir_all(tmp.path().join("z")).expect("mkdir z");
        std::fs::write(tmp.path().join("z/blocked"), "z\n").expect("write z/blocked");
        git(tmp.path(), &["add", "-A"]);
        git(tmp.path(), &["commit", "-qm", "base"]);

        git(tmp.path(), &["switch", "-q", "-c", "del"]);
        git(tmp.path(), &["rm", "-q", "a", "z/blocked"]);
        git(tmp.path(), &["commit", "-qm", "del"]);
        let del_oid =
            ObjectId::from_hex(git(tmp.path(), &["rev-parse", "del"]).trim()).expect("del oid");

        git(tmp.path(), &["switch", "-q", "main"]);
        let main_head = git(tmp.path(), &["rev-parse", "HEAD"]);

        chmod_dir_read_only(tmp.path(), "z");

        let repo = Repository::open(
            &tmp.path().join(".git"),
            &RepositoryOptions::empty().with_work_tree(tmp.path()),
        )
        .expect("open grit repo");

        let err = switch_to_del(&repo, del_oid);
        assert!(
            err.to_string().contains("z/blocked"),
            "expected blocked path in error, got: {err}"
        );

        assert_eq!(git(tmp.path(), &["rev-parse", "HEAD"]), main_head);
        assert!(
            tmp.path().join("a").is_file(),
            "earlier deletion must roll back"
        );
        assert!(
            tmp.path().join("z/blocked").is_file(),
            "blocked file must remain"
        );
        assert_clean_worktree(tmp.path());

        chmod_dir_writable(tmp.path(), "z");
    }

    #[test]
    fn switch_parallel_delete_rollback_on_later_unlink_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        init_repo(tmp.path());

        std::fs::write(tmp.path().join("a"), "a\n").expect("write a");
        for i in 0..35 {
            std::fs::write(
                tmp.path().join(format!("bulk{i:02}.txt")),
                format!("file {i}\n"),
            )
            .expect("write bulk file");
        }
        std::fs::create_dir_all(tmp.path().join("z")).expect("mkdir z");
        std::fs::write(tmp.path().join("z/blocked"), "z\n").expect("write z/blocked");
        git(tmp.path(), &["add", "-A"]);
        git(tmp.path(), &["commit", "-qm", "base"]);

        git(tmp.path(), &["switch", "-q", "-c", "del"]);
        let mut rm_args = vec!["rm", "-q", "a", "z/blocked"];
        let bulk_names: Vec<String> = (0..35).map(|i| format!("bulk{i:02}.txt")).collect();
        for name in &bulk_names {
            rm_args.push(name.as_str());
        }
        git(tmp.path(), &rm_args);
        git(tmp.path(), &["commit", "-qm", "del"]);
        let del_oid =
            ObjectId::from_hex(git(tmp.path(), &["rev-parse", "del"]).trim()).expect("del oid");

        git(tmp.path(), &["switch", "-q", "main"]);
        let main_head = git(tmp.path(), &["rev-parse", "HEAD"]);

        chmod_dir_read_only(tmp.path(), "z");

        let repo = Repository::open(
            &tmp.path().join(".git"),
            &RepositoryOptions::empty().with_work_tree(tmp.path()),
        )
        .expect("open grit repo");

        let err = switch_to_del(&repo, del_oid);
        assert!(
            err.to_string().contains("z/blocked"),
            "expected blocked path in error, got: {err}"
        );

        assert_eq!(git(tmp.path(), &["rev-parse", "HEAD"]), main_head);
        assert!(tmp.path().join("a").is_file());
        assert!(
            tmp.path().join("bulk00.txt").is_file(),
            "parallel deletions must roll back"
        );
        assert!(tmp.path().join("z/blocked").is_file());
        assert_clean_worktree(tmp.path());

        chmod_dir_writable(tmp.path(), "z");
    }
}
