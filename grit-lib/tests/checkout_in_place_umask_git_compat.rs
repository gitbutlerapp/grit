//! In-place regular-file checkout must preserve restrictive permissions like system `git`.

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use grit_lib::objects::ObjectId;
    use grit_lib::porcelain::checkout::checkout_between_trees;
    use grit_lib::repo::Repository;
    use grit_test_support::git;

    fn tree_of_head(repo: &std::path::Path) -> ObjectId {
        let hex = git(repo, &["rev-parse", "HEAD^{tree}"]).trim().to_owned();
        ObjectId::from_hex(&hex).expect("tree oid")
    }

    fn mode_bits(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    static UMASK_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Process umask is global; serialize these oracles so parallel tests do not race.
    fn with_umask<R>(mask: libc::mode_t, f: impl FnOnce() -> R) -> R {
        let _guard = UMASK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = unsafe { libc::umask(mask) };
        let out = f();
        unsafe {
            libc::umask(old);
        }
        out
    }

    #[test]
    fn in_place_content_only_checkout_preserves_umask_077_file_mode_like_git() {
        with_umask(0o077, || {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path();

            git(root, &["init", "-q", "-b", "main", "."]);
            git(root, &["config", "user.email", "t@example.com"]);
            git(root, &["config", "user.name", "Test"]);

            std::fs::write(root.join("secret.txt"), b"main\n").expect("write main");
            git(root, &["add", "secret.txt"]);
            git(root, &["commit", "-qm", "main"]);
            let main_tree = tree_of_head(root);
            let main_commit = git(root, &["rev-parse", "HEAD"]).trim().to_owned();

            std::fs::write(root.join("secret.txt"), b"target\n").expect("write target");
            git(root, &["add", "secret.txt"]);
            git(root, &["commit", "-qm", "target"]);
            let target_tree = tree_of_head(root);
            let target_commit = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
            git(root, &["branch", "target"]);

            git(root, &["reset", "--hard", &main_commit]);
            assert_eq!(
                mode_bits(&root.join("secret.txt")),
                0o600,
                "main checkout should leave 0600 under umask 077"
            );

            git(root, &["checkout", "-q", "target"]);
            let git_mode = mode_bits(&root.join("secret.txt"));
            assert_eq!(
                git_mode, 0o600,
                "git switch must preserve 0600 for content-only regular-file update"
            );

            git(root, &["reset", "--hard", &main_commit]);
            assert_eq!(mode_bits(&root.join("secret.txt")), 0o600);

            let grit_repo = Repository::open(&root.join(".git"), Some(root)).expect("open");
            checkout_between_trees(&grit_repo, Some(&main_tree), &target_tree).expect("checkout");
            grit_lib::refs::write_ref(
                &grit_repo.git_dir,
                "HEAD",
                &ObjectId::from_hex(&target_commit).expect("commit"),
            )
            .expect("head");

            let grit_mode = mode_bits(&root.join("secret.txt"));
            assert_eq!(
            grit_mode, git_mode,
            "grit in-place checkout must match git file mode (expected 0600, got {grit_mode:#o})"
        );
        });
    }

    fn checkout_mode_oracle(
        root: &std::path::Path,
        umask: libc::mode_t,
        prepare_main: Option<&dyn Fn(&std::path::Path)>,
        prepare_target: &dyn Fn(&std::path::Path),
        label: &str,
    ) {
        with_umask(umask, || {
            git(root, &["init", "-q", "-b", "main", "."]);
            git(root, &["config", "user.email", "t@example.com"]);
            git(root, &["config", "user.name", "Test"]);

            std::fs::write(root.join("secret.txt"), b"main\n").expect("write main");
            if let Some(prepare_main) = prepare_main {
                prepare_main(root);
            }
            git(root, &["add", "secret.txt"]);
            git(root, &["commit", "-qm", "main"]);
            let main_tree = tree_of_head(root);
            let main_commit = git(root, &["rev-parse", "HEAD"]).trim().to_owned();

            prepare_target(root);
            git(root, &["add", "secret.txt"]);
            git(root, &["commit", "-qm", "target"]);
            let target_tree = tree_of_head(root);
            let target_commit = git(root, &["rev-parse", "HEAD"]).trim().to_owned();
            git(root, &["branch", "target"]);

            git(root, &["reset", "--hard", &main_commit]);
            git(root, &["checkout", "-q", "target"]);
            let git_mode = mode_bits(&root.join("secret.txt"));

            git(root, &["reset", "--hard", &main_commit]);
            let grit_repo = Repository::open(&root.join(".git"), Some(root)).expect("open");
            checkout_between_trees(&grit_repo, Some(&main_tree), &target_tree).expect("checkout");
            grit_lib::refs::write_ref(
                &grit_repo.git_dir,
                "HEAD",
                &ObjectId::from_hex(&target_commit).expect("commit"),
            )
            .expect("head");
            let grit_mode = mode_bits(&root.join("secret.txt"));

            assert_eq!(
                grit_mode, git_mode,
                "{label}: grit mode {grit_mode:#o} must match git {git_mode:#o}"
            );
        });
    }

    #[test]
    fn in_place_mode_to_executable_matches_git_under_umask_077() {
        let tmp = tempfile::tempdir().expect("tempdir");
        checkout_mode_oracle(
            tmp.path(),
            0o077,
            None,
            &|root| {
                std::fs::write(root.join("secret.txt"), b"target\n").expect("write");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = std::fs::metadata(root.join("secret.txt"))
                        .expect("meta")
                        .permissions();
                    perms.set_mode(0o700);
                    std::fs::set_permissions(root.join("secret.txt"), perms).expect("chmod");
                }
            },
            "100644→100755 content+mode (umask 077)",
        );
    }

    #[test]
    fn in_place_mode_to_executable_matches_git_under_umask_022() {
        let tmp = tempfile::tempdir().expect("tempdir");
        checkout_mode_oracle(
            tmp.path(),
            0o022,
            None,
            &|root| {
                std::fs::write(root.join("secret.txt"), b"target\n").expect("write");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = std::fs::metadata(root.join("secret.txt"))
                        .expect("meta")
                        .permissions();
                    perms.set_mode(0o755);
                    std::fs::set_permissions(root.join("secret.txt"), perms).expect("chmod");
                }
            },
            "100644→100755 content+mode (umask 022)",
        );
    }

    #[test]
    fn in_place_mode_to_regular_matches_git_under_umask_077() {
        let tmp = tempfile::tempdir().expect("tempdir");
        checkout_mode_oracle(
            tmp.path(),
            0o077,
            Some(&|root| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = std::fs::metadata(root.join("secret.txt"))
                        .expect("meta")
                        .permissions();
                    perms.set_mode(0o700);
                    std::fs::set_permissions(root.join("secret.txt"), perms).expect("chmod");
                }
            }),
            &|root| {
                std::fs::write(root.join("secret.txt"), b"plain\n").expect("write");
            },
            "100755→100644 content+mode (umask 077)",
        );
    }
}
