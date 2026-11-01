//! Unreadable directories must warn and must not abort diff or stash (issue #2406).

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;

    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::diagnostics::{CollectingDiagnostics, DiagnosticsHandle, Warning};
    use grit_lib::diff::{DiffOptions, DiffTarget};
    use grit_lib::progress::{NeverCancel, NullProgress};
    use grit_lib::repo::Repository;
    use grit_lib::stash::StashPushOptions;
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

    fn init_pub_priv_base(root: &Path) {
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        fs::create_dir(root.join("pub")).expect("mkdir pub");
        fs::create_dir(root.join("priv")).expect("mkdir priv");
        fs::write(root.join("pub/a"), "a\n").expect("write pub/a");
        fs::write(root.join("priv/b"), "b\n").expect("write priv/b");
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
    }

    #[test]
    fn diff_shows_sibling_when_priv_directory_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_pub_priv_base(root);

        fs::write(root.join("pub/a"), "a\nmod\n").expect("edit pub/a");
        fs::write(root.join("priv/new.txt"), "new\n").expect("write priv/new.txt");
        set_mode(&root.join("priv"), 0o000);
        if fs::read_dir(root.join("priv")).is_ok() {
            set_mode(&root.join("priv"), 0o755);
            return;
        }

        let fx = open_repo(root);
        let files = fx
            .repo
            .diff()
            .files(&DiffTarget::WorktreeVsHead, &[], &DiffOptions::default())
            .expect("diff must succeed");
        assert!(
            files.iter().any(|f| f.path == "pub/a"),
            "pub/a diff expected: {:?}",
            files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        assert!(
            !files.iter().any(|p| p.path.starts_with("priv/")),
            "priv paths must not appear as modified/deleted: {:?}",
            files
        );

        let warnings = fx.diagnostics.warnings();
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                Warning::WorktreeAccessFailed { path, .. }
                    if path.contains("priv/b") || path.starts_with("priv/")
            )),
            "expected warning for priv/, got {warnings:?}"
        );

        let git_diff = git(root, &["diff", "pub/a"]);
        assert!(
            git_diff.contains("mod"),
            "system git should show pub/a hunk: {git_diff}"
        );

        set_mode(&root.join("priv"), 0o755);
    }

    #[test]
    fn diff_with_pathspec_skips_unreadable_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_pub_priv_base(root);

        fs::write(root.join("pub/a"), "a\nmod\n").expect("edit pub/a");
        set_mode(&root.join("priv"), 0o000);
        if fs::read_dir(root.join("priv")).is_ok() {
            set_mode(&root.join("priv"), 0o755);
            return;
        }

        let fx = open_repo(root);
        let files = fx
            .repo
            .diff()
            .files(
                &DiffTarget::WorktreeVsHead,
                &["pub".into()],
                &DiffOptions::default(),
            )
            .expect("pathspec diff must succeed");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "pub/a");
        assert!(
            fx.diagnostics.warnings().is_empty(),
            "pathspec limited to pub must not warn about priv"
        );

        set_mode(&root.join("priv"), 0o755);
    }

    #[test]
    fn stash_saves_readable_change_when_priv_unreadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        init_pub_priv_base(root);

        fs::write(root.join("pub/a"), "a\nstash\n").expect("edit pub/a");
        set_mode(&root.join("priv"), 0o000);
        if fs::read_dir(root.join("priv")).is_ok() {
            set_mode(&root.join("priv"), 0o755);
            return;
        }

        let fx = open_repo(root);
        let opts = StashPushOptions {
            message: None,
            include_untracked: false,
            identity: "Test <t@example.com> 1000000000 +0000"
                .parse()
                .expect("signature"),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: grit_lib::porcelain::stash::StashMode::default(),
        };
        fx.repo
            .stash()
            .push(&opts, &mut NullProgress, &NeverCancel)
            .expect("stash must succeed");

        set_mode(&root.join("priv"), 0o755);
        git(root, &["fsck"]);
        let stash_show = git(root, &["stash", "show", "-p"]);
        assert!(
            stash_show.contains("stash"),
            "stash should include pub/a change: {stash_show}"
        );
        let priv_b = fs::read_to_string(root.join("priv/b")).expect("priv/b readable again");
        assert_eq!(priv_b, "b\n");
    }
}
