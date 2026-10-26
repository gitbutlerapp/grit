//! Large blobs are streamed into the work tree by restore, switch and hard reset, from loose
//! objects, a single pack and several packs; the system `git` must see the result as clean
//! and the repository must pass `git fsck`. Converted paths (autocrlf) are not streamed.

use crate::support;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::repo::Repository;

/// Above the library's 1 MiB streaming limit.
const BIG: usize = 3 * 1024 * 1024 + 123;

#[test]
fn streamed_files_keep_cancellation_bounded_after_checkout_chunks_grow() {
    use grit_lib::porcelain::checkout::checkout_between_trees_with_progress;
    use grit_lib::progress::{FlagCancel, ProgressSink};

    struct StopAfterNine {
        completed: u64,
        cancel: FlagCancel,
    }

    impl ProgressSink for StopAfterNine {
        fn inc(&mut self, units: u64) {
            self.completed += units;
            if self.completed >= 9 {
                self.cancel.cancel();
            }
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "checkout.workers", "4"]);
    git(root, &["config", "checkout.thresholdForParallelism", "0"]);
    let first = root.join("f000.dat");
    fs::write(&first, vec![b'x'; BIG]).expect("large file");
    for i in 1..64 {
        fs::hard_link(&first, root.join(format!("f{i:03}.dat"))).expect("link large file");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "large files"]);
    let tree = ObjectId::from_hex(git(root, &["rev-parse", "HEAD^{tree}"]).trim()).expect("tree");
    git(root, &["rm", "-qr", "--", "."]);
    let repo = open(root);
    let cancel = FlagCancel::new();
    let mut progress = StopAfterNine {
        completed: 0,
        cancel: cancel.clone(),
    };
    let err = checkout_between_trees_with_progress(&repo, None, &tree, &mut progress, &cancel)
        .expect_err("cancel after the initial eight-file chunk");
    assert!(matches!(err, grit_lib::error::Error::Cancelled), "{err:?}");
    assert_eq!(progress.completed, 9);
    let written = (0..64)
        .filter(|i| root.join(format!("f{i:03}.dat")).is_file())
        .count();
    assert!(
        (9..=16).contains(&written),
        "streamed writes must stop within another eight files, wrote {written}"
    );
    git(root, &["fsck", "--strict", "--no-progress"]);
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut n = seed;
    while out.len() < len {
        out.extend_from_slice(format!("{n} some text in a large file\n").as_bytes());
        n = n.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(3) % 999_983;
    }
    out.truncate(len);
    out
}

#[derive(Clone, Copy, Debug)]
enum Storage {
    Loose,
    SinglePack,
    MultiPack,
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    big: Vec<u8>,
    big_exec: Vec<u8>,
    main: ObjectId,
    empty: ObjectId,
}

fn fixture(storage: Storage) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "gc.auto", "0"]);
    let big = payload(BIG, 7);
    let big_exec = payload(BIG / 2, 11);
    fs::write(root.join("small.txt"), "small\n").expect("small");
    fs::create_dir_all(root.join("assets")).expect("mkdir");
    fs::write(root.join("assets/big.dat"), &big).expect("big");
    fs::write(root.join("tool.sh"), &big_exec).expect("exec");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("tool.sh"), fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "big files"]);
    if matches!(storage, Storage::MultiPack) {
        git(&root, &["repack", "-dq"]);
    }
    git(&root, &["switch", "-qc", "empty"]);
    git(&root, &["rm", "-q", "assets/big.dat", "tool.sh"]);
    git(&root, &["commit", "-qm", "drop big files"]);
    match storage {
        Storage::Loose => {}
        Storage::SinglePack => git(&root, &["repack", "-adq"]).truncate(0),
        Storage::MultiPack => git(&root, &["repack", "-dq"]).truncate(0),
    }
    git(&root, &["switch", "-q", "main"]);
    let packs = fs::read_dir(root.join(".git/objects/pack"))
        .expect("pack dir")
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "pack"))
        })
        .count();
    let expected_packs = match storage {
        Storage::Loose => 0,
        Storage::SinglePack => 1,
        Storage::MultiPack => 2,
    };
    assert_eq!(packs, expected_packs, "{storage:?} fixture pack count");
    let main = ObjectId::from_hex(git(&root, &["rev-parse", "main"]).trim()).expect("main");
    let empty = ObjectId::from_hex(git(&root, &["rev-parse", "empty"]).trim()).expect("empty");
    Fixture {
        _dir: dir,
        root,
        big,
        big_exec,
        main,
        empty,
    }
}

fn open(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open repo")
}

fn assert_git_clean(fx: &Fixture, context: &str) {
    let status = git(&fx.root, &["status", "--porcelain"]);
    assert!(status.trim().is_empty(), "{context}: git status:\n{status}");
    git(&fx.root, &["fsck", "--no-progress"]);
    let big = fs::read(fx.root.join("assets/big.dat")).expect("read big");
    assert!(big == fx.big, "{context}: assets/big.dat content differs");
    let exec = fs::read(fx.root.join("tool.sh")).expect("read exec");
    assert!(exec == fx.big_exec, "{context}: tool.sh content differs");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(fx.root.join("tool.sh"))
            .expect("meta")
            .permissions()
            .mode();
        assert_ne!(
            mode & 0o100,
            0,
            "{context}: tool.sh lost its executable bit"
        );
        let mode = fs::metadata(fx.root.join("assets/big.dat"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o111,
            0,
            "{context}: assets/big.dat became executable"
        );
    }
}

fn restore_from_index(repo: &Repository, paths: &[&str]) {
    restore_paths(
        repo,
        &RestoreOptions {
            pathspecs: paths.iter().map(|p| (*p).to_owned()).collect(),
            pathspec_sources: paths.iter().map(|p| (*p).to_owned()).collect(),
            source: RestoreSource::Index,
            staged: false,
            worktree: true,
            stage: None,
            remerge: None,
        },
    )
    .expect("restore");
}

fn switch_to(repo: &Repository, branch: &str, oid: ObjectId) {
    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: branch.into(),
                oid,
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("switch");
}

fn exercise(storage: Storage) {
    let fx = fixture(storage);
    let repo = open(&fx.root);

    fs::remove_file(fx.root.join("assets/big.dat")).expect("rm big");
    fs::remove_file(fx.root.join("tool.sh")).expect("rm exec");
    restore_from_index(&repo, &["assets/big.dat", "tool.sh"]);
    assert_git_clean(&fx, &format!("{storage:?} restore"));

    switch_to(&repo, "empty", fx.empty);
    assert!(!fx.root.join("assets/big.dat").exists());
    switch_to(&repo, "main", fx.main);
    assert_git_clean(&fx, &format!("{storage:?} switch"));

    fs::write(fx.root.join("assets/big.dat"), b"scribbled over\n").expect("modify");
    fs::remove_file(fx.root.join("tool.sh")).expect("rm exec");
    reset_to_commit(
        &repo,
        &ResetOptions::new(
            fx.main,
            ResetMode::Hard,
            support::parse_reflog_signature("T <t@example.com>"),
            "reset: test",
        ),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("reset --hard");
    assert_git_clean(&fx, &format!("{storage:?} reset --hard"));
}

#[test]
fn large_blobs_from_loose_objects_check_out_clean() {
    exercise(Storage::Loose);
}

#[test]
fn large_blobs_from_a_single_pack_check_out_clean() {
    exercise(Storage::SinglePack);
}

#[test]
fn large_blobs_from_several_packs_check_out_clean() {
    exercise(Storage::MultiPack);
}

/// Truncate the loose object behind `path` in `HEAD` to half its compressed size.
fn truncate_loose_blob(root: &Path, path: &str) {
    let oid = git(root, &["rev-parse", &format!("HEAD:{path}")]);
    let oid = oid.trim();
    let loose = root.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
    let bytes = fs::read(&loose).expect("read loose object");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).expect("chmod");
    }
    fs::write(&loose, &bytes[..bytes.len() / 2]).expect("truncate loose object");
}

#[test]
fn corrupt_large_blob_never_leaves_a_partial_file() {
    let fx = fixture(Storage::Loose);
    truncate_loose_blob(&fx.root, "assets/big.dat");
    let repo = open(&fx.root);

    fs::remove_file(fx.root.join("assets/big.dat")).expect("rm big");
    let err = restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["assets/big.dat".into()],
            pathspec_sources: vec!["assets/big.dat".into()],
            source: RestoreSource::Index,
            staged: false,
            worktree: true,
            stage: None,
            remerge: None,
        },
    );
    assert!(err.is_err(), "restore of a truncated blob must fail");
    assert!(
        !fx.root.join("assets/big.dat").exists(),
        "a failed restore must not leave a partial file"
    );

    fs::write(fx.root.join("assets/big.dat"), b"local edit\n").expect("modify");
    let err = reset_to_commit(
        &repo,
        &ResetOptions::new(
            fx.main,
            ResetMode::Hard,
            support::parse_reflog_signature("T <t@example.com>"),
            "reset: test",
        ),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    );
    assert!(err.is_err(), "reset --hard to a truncated blob must fail");
    assert_eq!(
        fs::read(fx.root.join("assets/big.dat")).expect("read"),
        b"local edit\n",
        "a failed reset must leave the existing file untouched"
    );
    let leftovers: Vec<_> = fs::read_dir(fx.root.join("assets"))
        .expect("read assets")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "big.dat")
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

#[test]
fn large_text_blob_under_autocrlf_is_converted_not_copied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    let text = payload(BIG, 5);
    let lines = text.iter().filter(|&&b| b == b'\n').count();
    {
        let mut file = fs::File::create(root.join("big.txt")).expect("create");
        file.write_all(&text).expect("write");
    }
    git(root, &["add", "big.txt"]);
    git(root, &["commit", "-qm", "text"]);
    git(root, &["config", "core.autocrlf", "true"]);

    fs::remove_file(root.join("big.txt")).expect("rm");
    let repo = open(root);
    restore_paths(
        &repo,
        &RestoreOptions {
            pathspecs: vec!["big.txt".into()],
            pathspec_sources: vec!["big.txt".into()],
            source: RestoreSource::Index,
            staged: false,
            worktree: true,
            stage: None,
            remerge: None,
        },
    )
    .expect("restore");

    let restored = fs::read(root.join("big.txt")).expect("read");
    assert_eq!(restored.len(), text.len() + lines, "every LF becomes CRLF");
    assert!(restored.windows(2).any(|w| w == b"\r\n"));
    let status = git(root, &["status", "--porcelain"]);
    assert!(
        status.trim().is_empty(),
        "git status after autocrlf restore:\n{status}"
    );
}
