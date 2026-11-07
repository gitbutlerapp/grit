//! Work-tree scan behavior checked against the system `git`: symlinks in parallel scans and
//! the staging scan, and `skip_index_path` combined with `restrict_to_paths`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[cfg(unix)]
mod parallel_symlinks {
    //! Parallel worktree scans compare symlinks as link-target blobs (#2076).

    use crate::objects::FileMode;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use crate::config::environment::RepositoryOptions;
    use crate::diff::{
        diff_index_to_worktree_for_staging, diff_index_to_worktree_with_options, DiffEntry,
        DiffIndexToWorktreeOptions,
    };
    use crate::index::Index;
    use crate::parallel::ThreadPolicy;
    use crate::repo::Repository;
    use grit_test_support::git;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "user.email", "test@example.com"]);
        fs::create_dir(root.join("files")).expect("files directory");
        for n in 0..1024 {
            fs::write(root.join(format!("files/{n}")), b"data\n").expect("ordinary file");
        }
        for (name, target) in [
            ("linkdir", "files"),
            ("linkfile", "files/0"),
            ("linkmissing", "missing"),
        ] {
            symlink(target, root.join(name)).expect("symlink");
        }
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "initial"]);
        tmp
    }

    fn open(root: &Path) -> Repository {
        Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("repository")
    }

    #[derive(Clone, Copy, Debug)]
    enum StatState {
        Unset,
        Stale,
        Racy,
    }

    fn force_verification(index: &mut Index, state: StatState) -> Option<(u32, u32)> {
        for entry in index.entries_mut() {
            match state {
                StatState::Unset => {
                    entry.mtime_sec = 0;
                    entry.mtime_nsec = 0;
                    entry.size = 0;
                }
                StatState::Stale => entry.mtime_sec = entry.mtime_sec.wrapping_sub(10),
                StatState::Racy => {}
            }
        }
        index.sort();
        // Every recorded mtime is at or after this boundary, without sleeps or clock races.
        matches!(state, StatState::Racy).then_some((1, 0))
    }

    fn git_statuses(root: &Path) -> BTreeMap<String, char> {
        git(root, &["diff", "--no-renames", "--name-status"])
            .lines()
            .map(|line| {
                let (status, path) = line.split_once('\t').expect("name status");
                (path.to_owned(), status.chars().next().expect("status"))
            })
            .collect()
    }

    fn statuses(entries: &[DiffEntry]) -> BTreeMap<String, char> {
        entries
            .iter()
            .map(|entry| (entry.path().to_owned(), entry.status.letter()))
            .collect()
    }

    #[derive(Clone, Copy, Debug)]
    enum Scan {
        Diff,
        Status,
        Preview,
        Stage,
    }

    /// Exercise both public scans, one and four threads, and all forced stat states.
    fn check_scans(root: &Path) {
        let expected = git_statuses(root);
        let repo = open(root);
        let original = repo
            .index()
            .load(&crate::index::IndexLoadOptions::default())
            .expect("index");
        let index_bytes = fs::read(root.join(".git/index")).expect("disk index");
        for state in [StatState::Unset, StatState::Stale, StatState::Racy] {
            for refresh in [false, true] {
                for threads in [1, 4] {
                    // Hash-only comes before materialization to prove it writes no missing blobs.
                    for scan in [Scan::Diff, Scan::Status, Scan::Preview, Scan::Stage] {
                        let mut index = original.clone();
                        let index_mtime = force_verification(&mut index, state);
                        let options = DiffIndexToWorktreeOptions {
                            repository_git_dir: Some(repo.git_dir().to_owned()),
                            thread_policy: Some(ThreadPolicy::with_threads_override(Some(threads))),
                            index_mtime,
                            refresh_index_stat_in_pass: refresh,
                            hash_blobs_without_write: matches!(scan, Scan::Preview),
                            // Status enables parallel content hashing on the non-staging API.
                            // An unset cache also forces status past its size-only shortcut.
                            for_status: matches!(scan, Scan::Status),
                            refresh_fsmonitor: false,
                            ..Default::default()
                        };
                        let before = object_files(&root.join(".git/objects"));
                        let entries = if matches!(scan, Scan::Diff | Scan::Status) {
                            diff_index_to_worktree_with_options(
                                repo.odb(),
                                &mut index,
                                root,
                                options,
                            )
                            .expect("worktree diff")
                            .0
                        } else {
                            diff_index_to_worktree_for_staging(
                                repo.odb(),
                                repo.git_dir(),
                                &mut index,
                                root,
                                options,
                            )
                            .expect("staging scan")
                            .0
                        };
                        assert_eq!(
                            statuses(&entries),
                            expected,
                            "{state:?}, refresh={refresh}, threads={threads}, scan={scan:?}"
                        );
                        if !matches!(scan, Scan::Stage) {
                            assert_eq!(object_files(&root.join(".git/objects")), before);
                        }
                        if matches!(scan, Scan::Preview | Scan::Stage) {
                            for entry in &entries {
                                if entry.new_mode == FileMode::SYMLINK {
                                    let path = root.join(entry.path());
                                    let bytes = if fs::symlink_metadata(&path)
                                        .expect("metadata")
                                        .file_type()
                                        .is_symlink()
                                    {
                                        fs::read_link(path)
                                            .expect("target")
                                            .as_os_str()
                                            .as_bytes()
                                            .to_vec()
                                    } else {
                                        fs::read(path).expect("placeholder")
                                    };
                                    let bytes = bytes.as_slice();
                                    let oid = repo
                                        .odb()
                                        .hash_algo()
                                        .hash_object(crate::objects::ObjectKind::Blob, bytes)
                                        .expect("link hash");
                                    assert_eq!(entry.new_oid, oid, "link-target bytes");
                                    if matches!(scan, Scan::Stage) {
                                        assert_eq!(
                                            repo.odb().read(&oid).expect("stored link").data,
                                            bytes
                                        );
                                        assert_eq!(
                                            git(root, &["cat-file", "blob", &oid.to_hex()])
                                                .as_bytes(),
                                            bytes
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(
            fs::read(root.join(".git/index")).expect("disk index"),
            index_bytes
        );
        git(root, &["fsck", "--strict"]);
    }

    fn object_files(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();
        for item in fs::read_dir(dir).expect("object directory") {
            let path = item.expect("object entry").path();
            if path.is_dir() {
                paths.extend(object_files(&path));
            } else {
                paths.push(path);
            }
        }
        paths.sort();
        paths
    }

    #[test]
    fn clean_directory_file_and_dangling_symlinks_match_git() {
        let tmp = fixture();
        assert!(git_statuses(tmp.path()).is_empty());
        check_scans(tmp.path());
    }

    #[test]
    fn retargeted_symlinks_hash_target_bytes_without_conversion() {
        let tmp = fixture();
        let root = tmp.path();
        // Rules apply only to links. Neither clean filters nor CRLF conversion may touch targets.
        fs::write(
            root.join(".git/info/attributes"),
            "link* text filter=reject\n",
        )
        .expect("attributes");
        git(root, &["config", "core.autocrlf", "true"]);
        git(root, &["config", "filter.reject.clean", "false"]);
        git(root, &["config", "filter.reject.required", "true"]);
        for (name, target) in [
            ("linkdir", "."),
            ("linkfile", "files/1"),
            ("linkmissing", "absent\r\n"),
        ] {
            fs::remove_file(root.join(name)).expect("remove link");
            symlink(target, root.join(name)).expect("retarget link");
        }
        assert_eq!(git_statuses(root).len(), 3);
        check_scans(root);
    }

    #[test]
    fn regular_file_and_symlink_type_changes_match_git() {
        let tmp = fixture();
        let root = tmp.path();
        fs::remove_file(root.join("files/0")).expect("remove regular file");
        symlink("1", root.join("files/0")).expect("file to link");
        fs::remove_file(root.join("linkfile")).expect("remove link");
        fs::write(root.join("linkfile"), b"regular replacement\n").expect("link to file");
        assert_eq!(
            git_statuses(root).values().copied().collect::<Vec<_>>(),
            vec!['T', 'T']
        );
        check_scans(root);
    }

    #[test]
    fn ordinary_directory_replacements_are_deletions() {
        let tmp = fixture();
        let root = tmp.path();
        for name in ["files/0", "linkdir"] {
            fs::remove_file(root.join(name)).expect("remove tracked path");
            fs::create_dir(root.join(name)).expect("replace with directory");
        }
        assert_eq!(
            git_statuses(root).values().copied().collect::<Vec<_>>(),
            vec!['D', 'D']
        );
        check_scans(root);
    }

    #[test]
    fn symlinks_false_regular_placeholders_keep_link_mode() {
        let tmp = fixture();
        let root = tmp.path();
        git(root, &["config", "core.symlinks", "false"]);
        for name in ["linkdir", "linkfile", "linkmissing"] {
            let target = fs::read_link(root.join(name)).expect("target");
            fs::remove_file(root.join(name)).expect("remove symlink");
            fs::write(root.join(name), target.as_os_str().as_bytes()).expect("placeholder");
        }
        assert!(git_statuses(root).is_empty());
        check_scans(root);
        fs::write(root.join("linkdir"), b"other directory").expect("edit placeholder");
        check_scans(root);
    }

    #[test]
    fn uninitialized_gitlink_stays_on_its_separate_scan_path() {
        let tmp = fixture();
        let root = tmp.path();
        for name in ["linkdir", "linkfile", "linkmissing"] {
            fs::remove_file(root.join(name)).expect("remove control symlink");
        }
        git(root, &["add", "-u"]);
        let head = git(root, &["rev-parse", "HEAD"]);
        git(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{},submodule", head.trim()),
            ],
        );
        git(root, &["commit", "-qm", "gitlink"]);
        fs::create_dir(root.join("submodule")).expect("uninitialized checkout");
        assert!(git_statuses(root).is_empty());
        check_scans(root);
    }
}

mod skip_restrict_index_paths {
    //! `skip_index_path` must apply together with `restrict_to_paths` on regular files.

    use std::collections::BTreeSet;
    use std::sync::Arc;

    use crate::config::environment::RepositoryOptions;
    use crate::diff::{diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions};
    use crate::repo::Repository;
    use grit_test_support::git;

    fn diff_paths_with_options(
        root: &std::path::Path,
        restrict: BTreeSet<String>,
        skip: Arc<dyn Fn(&[u8]) -> bool + Send + Sync>,
    ) -> BTreeSet<String> {
        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(root),
        )
        .expect("open");
        let mut index = repo
            .index()
            .load(&crate::index::IndexLoadOptions::default())
            .expect("index");
        let index_mtime = crate::diff::index_mtime_for_diff(&index, None);
        let config = repo.config().snapshot().expect("config");
        let (entries, _) = diff_index_to_worktree_with_options(
            repo.odb(),
            &mut index,
            root,
            DiffIndexToWorktreeOptions {
                repository_git_dir: Some(repo.git_dir().to_path_buf()),
                index_mtime,
                refresh_index_stat_in_pass: true,
                refresh_fsmonitor: false,
                for_status: true,
                config: Some(config),
                thread_policy: Some(repo.thread_policy()),
                restrict_to_paths: Some(Arc::new(restrict)),
                skip_index_path: Some(skip),
                ..DiffIndexToWorktreeOptions::default()
            },
        )
        .expect("diff");
        entries
            .into_iter()
            .filter_map(|e| e.old_path.or(e.new_path))
            .collect()
    }

    #[test]
    fn skip_index_path_applies_to_restricted_regular_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git(root, &["init", "-q", "-b", "main", "."]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        std::fs::write(root.join("keep.txt"), "keep v1\n").expect("write");
        std::fs::write(root.join("skip.txt"), "skip v1\n").expect("write");
        git(root, &["add", "keep.txt", "skip.txt"]);
        git(root, &["commit", "-qm", "base"]);
        std::fs::write(root.join("keep.txt"), "keep v2\n").expect("modify keep");
        std::fs::write(root.join("skip.txt"), "skip v2\n").expect("modify skip");

        let restrict: BTreeSet<String> = ["keep.txt", "skip.txt"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let skip = Arc::new(|path: &[u8]| path.ends_with(b"skip.txt"))
            as Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

        let paths = diff_paths_with_options(root, restrict.clone(), Arc::clone(&skip));
        assert_eq!(paths, BTreeSet::from(["keep.txt".to_owned()]));

        let skip_none = Arc::new(|_path: &[u8]| false) as Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;
        let all = diff_paths_with_options(root, restrict, skip_none);
        assert_eq!(
            all,
            BTreeSet::from(["keep.txt".to_owned(), "skip.txt".to_owned()])
        );
    }
}
