//! A checkout that is cancelled (or fails) part way leaves refs, the index and the work tree as
//! they were: the system `git` sees a clean tree, and retrying the same checkout succeeds.
//!
//! Each fixture switches between two commits that add, modify and delete files, remove a whole
//! directory, and (in the mixed fixture) change symlinks, executables and gitlinks. The cancel is
//! raised at many points of the apply, from the first file to the last.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees_with_progress;
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{
    Cancel, FlagCancel, NeverCancel, NullProgress, ProgressPhase, ProgressSink,
};
use grit_lib::repo::Repository;

/// Raises `cancel` on the `limit`-th checkout progress increment, after running `at_cancel`.
struct CancelAfter<F: FnMut()> {
    cancel: FlagCancel,
    limit: u64,
    seen: u64,
    in_checkout: bool,
    at_cancel: F,
}

impl<F: FnMut()> ProgressSink for CancelAfter<F> {
    fn start(&mut self, phase: ProgressPhase, _total: Option<u64>) {
        self.in_checkout = phase == ProgressPhase::CheckingOutFiles;
    }

    fn inc(&mut self, units: u64) {
        if !self.in_checkout || self.cancel.is_cancelled() {
            return;
        }
        self.seen += units;
        if self.seen >= self.limit {
            (self.at_cancel)();
            self.cancel.cancel();
        }
    }
}

/// Counts checkout progress increments of a full, uncancelled checkout.
#[derive(Default)]
struct CountIncs {
    in_checkout: bool,
    seen: u64,
}

impl ProgressSink for CountIncs {
    fn start(&mut self, phase: ProgressPhase, _total: Option<u64>) {
        self.in_checkout = phase == ProgressPhase::CheckingOutFiles;
    }

    fn inc(&mut self, units: u64) {
        if self.in_checkout {
            self.seen += units;
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, contents).expect("write");
}

#[cfg(unix)]
fn set_exec(root: &Path, rel: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(root.join(rel), fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    /// Regular files only: the bulk add/modify/delete apply.
    RegularFiles,
    /// Symlinks, executables, a type change and gitlinks too: the general apply.
    Mixed,
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    main: ObjectId,
    next: ObjectId,
}

/// `main` has `keep/` (modified on `next`), `gone/` (deleted wholesale on `next`) and `mixed/`
/// (half deleted on `next`); `next` adds `new/`. `HEAD` is `main`, checked out by `git`.
fn fixture(shape: Shape) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "gc.auto", "0"]);
    for i in 0..60 {
        write(&root, &format!("keep/f{i:02}.txt"), &format!("keep {i}\n"));
    }
    for i in 0..40 {
        write(&root, &format!("gone/g{i:02}.txt"), &format!("gone {i}\n"));
    }
    write(&root, "gone/nested/deep.txt", "deep\n");
    for i in 0..20 {
        write(
            &root,
            &format!("mixed/m{i:02}.txt"),
            &format!("mixed {i}\n"),
        );
    }
    write(&root, "top.txt", "top\n");
    if shape == Shape::Mixed {
        std::os::unix::fs::symlink("keep/f00.txt", root.join("link")).expect("symlink");
        write(&root, "tool.sh", "#!/bin/sh\necho tool\n");
        set_exec(&root, "tool.sh");
        write(&root, "run.sh", "#!/bin/sh\necho run\n");
        set_exec(&root, "run.sh");
        write(&root, "becomes-link", "regular before\n");
        std::os::unix::fs::symlink("top.txt", root.join("gone/link")).expect("symlink");
    }
    git(&root, &["add", "-A"]);
    if shape == Shape::Mixed {
        let sub = "1111111111111111111111111111111111111111";
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{sub},sub"),
            ],
        );
        fs::create_dir(root.join("sub")).expect("gitlink dir");
    }
    git(&root, &["commit", "-qm", "main"]);

    git(&root, &["switch", "-qc", "next"]);
    for i in 0..60 {
        write(
            &root,
            &format!("keep/f{i:02}.txt"),
            &format!("keep {i} changed\n"),
        );
    }
    git(&root, &["rm", "-rq", "gone"]);
    for i in (0..20).step_by(2) {
        git(&root, &["rm", "-q", &format!("mixed/m{i:02}.txt")]);
    }
    for i in 0..50 {
        write(
            &root,
            &format!("new/deep/n{i:02}.txt"),
            &format!("new {i}\n"),
        );
    }
    write(&root, "added.txt", "added\n");
    if shape == Shape::Mixed {
        git(&root, &["rm", "-q", "tool.sh", "sub"]);
        write(&root, "run.sh", "#!/bin/sh\necho run changed\n");
        fs::remove_file(root.join("becomes-link")).expect("rm");
        std::os::unix::fs::symlink("top.txt", root.join("becomes-link")).expect("symlink");
        fs::remove_file(root.join("link")).expect("rm link");
        std::os::unix::fs::symlink("keep/f01.txt", root.join("link")).expect("symlink");
        std::os::unix::fs::symlink("added.txt", root.join("new/link")).expect("symlink");
        let newsub = "2222222222222222222222222222222222222222";
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{newsub},newsub"),
            ],
        );
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "next"]);
    git(&root, &["switch", "-q", "main"]);
    if shape == Shape::Mixed {
        fs::create_dir_all(root.join("sub")).expect("gitlink dir");
    }
    let main = ObjectId::from_hex(git(&root, &["rev-parse", "main"]).trim()).expect("main");
    let next = ObjectId::from_hex(git(&root, &["rev-parse", "next"]).trim()).expect("next");
    assert_clean(&root, "fixture");
    Fixture {
        _dir: dir,
        root,
        main,
        next,
    }
}

fn open(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open repo")
}

fn detach_to(commit: ObjectId) -> SwitchOptions {
    SwitchOptions {
        mode: SwitchMode::Detached {
            commit,
            reflog_to_label: "next".into(),
        },
        dry_run: false,
        hooks: Default::default(),
    }
}

/// Bytes of `HEAD`, the branch it names, and the index: what a failed checkout must not touch.
fn repository_state(root: &Path) -> Vec<Vec<u8>> {
    ["HEAD", "refs/heads/main", "index"]
        .iter()
        .map(|rel| fs::read(root.join(".git").join(rel)).unwrap_or_default())
        .collect()
}

/// `git` sees no change, untracked file or ignored leftover, and the repository is sound.
fn assert_clean(root: &Path, context: &str) {
    let status = git(
        root,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
        ],
    );
    assert!(status.is_empty(), "{context}: git status:\n{status}");
    git(root, &["fsck", "--no-progress", "--no-dangling"]);
}

/// The `main` tree as the fixture laid it out, including what `git status` cannot see.
fn assert_main_layout(root: &Path, shape: Shape, context: &str) {
    assert!(!root.join("new").exists(), "{context}: new/ left behind");
    assert!(
        !root.join("added.txt").exists(),
        "{context}: added.txt left"
    );
    assert_eq!(
        fs::read_to_string(root.join("gone/nested/deep.txt"))
            .ok()
            .as_deref(),
        Some("deep\n"),
        "{context}: wholesale-removed directory not restored"
    );
    if shape != Shape::Mixed {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::read_link(root.join("link")).ok(),
        Some(PathBuf::from("keep/f00.txt")),
        "{context}: symlink target"
    );
    assert_eq!(
        fs::read_link(root.join("gone/link")).ok(),
        Some(PathBuf::from("top.txt")),
        "{context}: symlink inside the wholesale-removed directory"
    );
    for exec in ["tool.sh", "run.sh"] {
        let mode = fs::metadata(root.join(exec))
            .expect(exec)
            .permissions()
            .mode();
        assert_ne!(mode & 0o100, 0, "{context}: {exec} lost its executable bit");
    }
    assert!(
        fs::symlink_metadata(root.join("becomes-link"))
            .expect("becomes-link")
            .is_file(),
        "{context}: type change not undone"
    );
    assert!(
        root.join("sub").is_dir(),
        "{context}: gitlink directory removed"
    );
    assert!(
        !root.join("newsub").exists(),
        "{context}: new gitlink left behind"
    );
}

/// Checkout progress increments of a full switch from `main` to `next`, measured on a copy.
fn full_switch_incs(fx: &Fixture) -> u64 {
    let copy = tempfile::tempdir().expect("tempdir");
    let status = Command::new("cp")
        .args(["-a", "."])
        .arg(copy.path())
        .current_dir(&fx.root)
        .status()
        .expect("cp");
    assert!(status.success());
    let mut count = CountIncs::default();
    switch_repository(
        &open(copy.path()),
        &detach_to(fx.next),
        &mut count,
        &NeverCancel,
    )
    .expect("full switch on the copy");
    count.seen
}

fn cut_points(total: u64) -> Vec<u64> {
    let mut points = vec![
        1,
        2,
        8,
        9,
        total / 4,
        total / 2,
        total * 3 / 4,
        total - 1,
        total,
    ];
    points.retain(|&p| p >= 1 && p <= total);
    points.sort_unstable();
    points.dedup();
    points
}

fn cancelled_switch_rolls_back(shape: Shape) {
    let fx = fixture(shape);
    let total = full_switch_incs(&fx);
    assert!(
        total > 50,
        "{shape:?}: fixture too small ({total} increments)"
    );
    for limit in cut_points(total) {
        let context = format!("{shape:?} cancel at {limit}/{total}");
        let before = repository_state(&fx.root);
        let cancel = FlagCancel::new();
        let mut progress = CancelAfter {
            cancel: cancel.clone(),
            limit,
            seen: 0,
            in_checkout: false,
            at_cancel: || {},
        };
        let err = switch_repository(&open(&fx.root), &detach_to(fx.next), &mut progress, &cancel)
            .expect_err(&context);
        assert!(matches!(err, Error::Cancelled), "{context}: {err:?}");
        assert!(progress.seen >= limit, "{context}: cancel never fired");
        assert_eq!(
            repository_state(&fx.root),
            before,
            "{context}: HEAD or index moved"
        );
        assert_main_layout(&fx.root, shape, &context);
        assert_clean(&fx.root, &context);
    }

    let outcome = switch_repository(
        &open(&fx.root),
        &detach_to(fx.next),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("retry after cancelled switches");
    assert_eq!(outcome.commit, fx.next);
    assert_eq!(
        git(&fx.root, &["rev-parse", "HEAD"]).trim(),
        fx.next.to_hex()
    );
    assert_clean(&fx.root, &format!("{shape:?} retry"));
    assert_eq!(
        fs::read_to_string(fx.root.join("new/deep/n00.txt"))
            .ok()
            .as_deref(),
        Some("new 0\n")
    );
    assert!(!fx.root.join("gone").exists());
    assert_ne!(fx.main, fx.next);
}

#[test]
fn cancelled_switch_with_regular_files_leaves_repository_unchanged() {
    cancelled_switch_rolls_back(Shape::RegularFiles);
}

#[cfg(unix)]
#[test]
fn cancelled_switch_with_symlinks_executables_and_gitlinks_leaves_repository_unchanged() {
    cancelled_switch_rolls_back(Shape::Mixed);
}

fn tree_of(root: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(root, &["rev-parse", &format!("{rev}^{{tree}}")]).trim()).expect("tree")
}

#[cfg(unix)]
#[test]
fn cancelled_tree_to_tree_checkout_leaves_repository_unchanged() {
    let fx = fixture(Shape::Mixed);
    let from = tree_of(&fx.root, "main");
    let to = tree_of(&fx.root, "next");
    for limit in [1, 10, 40, 120] {
        let context = format!("checkout_between_trees cancel at {limit}");
        let before = repository_state(&fx.root);
        let cancel = FlagCancel::new();
        let mut progress = CancelAfter {
            cancel: cancel.clone(),
            limit,
            seen: 0,
            in_checkout: false,
            at_cancel: || {},
        };
        let err = checkout_between_trees_with_progress(
            &open(&fx.root),
            Some(&from),
            &to,
            &mut progress,
            &cancel,
        )
        .expect_err(&context);
        assert!(matches!(err, Error::Cancelled), "{context}: {err:?}");
        assert_eq!(repository_state(&fx.root), before, "{context}");
        assert_main_layout(&fx.root, Shape::Mixed, &context);
        assert_clean(&fx.root, &context);
    }
}

#[test]
fn cancelled_initial_checkout_leaves_work_tree_empty() {
    let fx = fixture(Shape::RegularFiles);
    let dest = tempfile::tempdir().expect("tempdir");
    let target = dest.path().join("clone");
    git(
        dest.path(),
        &[
            "clone",
            "-q",
            "--no-checkout",
            fx.root.to_str().expect("utf-8"),
            "clone",
        ],
    );
    let tree = tree_of(&target, "HEAD");
    let cancel = FlagCancel::new();
    let mut progress = CancelAfter {
        cancel: cancel.clone(),
        limit: 30,
        seen: 0,
        in_checkout: false,
        at_cancel: || {},
    };
    let err =
        checkout_between_trees_with_progress(&open(&target), None, &tree, &mut progress, &cancel)
            .expect_err("cancelled initial checkout");
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    let left: Vec<_> = fs::read_dir(&target)
        .expect("read clone")
        .map(|e| e.expect("entry").file_name())
        .filter(|name| name != ".git")
        .collect();
    assert!(left.is_empty(), "initial checkout left {left:?}");
    assert!(!target.join(".git/index").exists(), "index written");

    checkout_between_trees_with_progress(
        &open(&target),
        None,
        &tree,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("retry");
    let status = git(&target, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(status.is_empty(), "after retry:\n{status}");
}

/// When a path cannot be put back, the error says which, wraps the cancellation, and leaves
/// `HEAD` and the index alone.
#[cfg(unix)]
#[test]
fn rollback_that_cannot_remove_a_created_file_reports_it() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture(Shape::RegularFiles);
    let probe = fx.root.join("probe-dir");
    fs::create_dir(&probe).expect("probe");
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o555)).expect("chmod");
    let enforced = fs::write(probe.join("x"), "x").is_err();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).expect("chmod");
    fs::remove_dir_all(&probe).expect("rm probe");
    if !enforced {
        return;
    }

    // Paths are written in order, so `new/deep/` is being filled near the end.
    let limit = full_switch_incs(&fx) - 2;
    let locked = fx.root.join("new/deep");
    let before = repository_state(&fx.root);
    let cancel = FlagCancel::new();
    let mut progress = CancelAfter {
        cancel: cancel.clone(),
        limit,
        seen: 0,
        in_checkout: false,
        at_cancel: || {
            if locked.is_dir() {
                fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("lock");
            }
        },
    };
    let err = switch_repository(&open(&fx.root), &detach_to(fx.next), &mut progress, &cancel)
        .expect_err("cancelled switch");
    if locked.is_dir() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("unlock");
    }
    let Error::CheckoutRollbackIncomplete { source, unrestored } = &err else {
        panic!("expected an incomplete rollback, got {err:?}");
    };
    assert!(matches!(**source, Error::Cancelled), "{source:?}");
    assert!(err.is_cancelled());
    assert!(!unrestored.is_empty());
    assert!(
        unrestored.iter().all(|p| p.starts_with("new/deep/n")),
        "unexpected unrestored paths {unrestored:?}"
    );
    assert!(
        unrestored.windows(2).all(|w| w[0] < w[1]),
        "sorted and unique"
    );
    for rel in unrestored {
        assert!(fx.root.join(rel).is_file(), "{rel} reported but absent");
    }
    assert_eq!(repository_state(&fx.root), before, "HEAD or index moved");
    assert!(fx.root.join("gone/nested/deep.txt").is_file());
    let status = git(
        &fx.root,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    let untracked: Vec<&str> = status.lines().collect();
    assert_eq!(
        untracked.len(),
        unrestored.len(),
        "only the reported paths differ:\n{status}"
    );
}
