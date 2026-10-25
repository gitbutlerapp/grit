//! Switch must fail when a checkout deletion cannot remove a file (issue #1968).

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::error::{Error, WorktreePathOperation};
    use grit_lib::objects::ObjectId;
    use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
    use grit_lib::repo::Repository;
    use grit_test_support::git;

    #[test]
    fn switch_fails_when_deleted_path_cannot_be_unlinked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        git(tmp.path(), &["init", "-q", "-b", "main", "."]);
        git(tmp.path(), &["config", "user.email", "t@example.com"]);
        git(tmp.path(), &["config", "user.name", "Test"]);

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

        let mut perms = std::fs::metadata(tmp.path().join("d"))
            .expect("meta d")
            .permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(tmp.path().join("d"), perms).expect("chmod d");

        let repo = Repository::open(
            &tmp.path().join(".git"),
            &RepositoryOptions::empty().with_work_tree(tmp.path()),
        )
        .expect("open grit repo");

        let err = switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "del".into(),
                    oid: del_oid,
                },
                dry_run: false,
                hooks: grit_lib::PostOperationHooks::Skip,
            },
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect_err("switch must fail when d/a cannot be removed");

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

        assert_eq!(
            git(tmp.path(), &["rev-parse", "HEAD"]),
            main_head,
            "HEAD must stay on main after failed switch"
        );
        assert!(
            tmp.path().join("d/a").is_file(),
            "d/a must remain on disk when removal fails"
        );

        let mut perms = std::fs::metadata(tmp.path().join("d"))
            .expect("meta d")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(tmp.path().join("d"), perms).expect("restore d perms");
    }
}
