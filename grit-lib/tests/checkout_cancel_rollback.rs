//! A checkout that is cancelled (or fails) part way leaves refs, the index and the work tree as
//! they were: the system `git` sees a clean tree, and retrying the same checkout succeeds.
//!
//! Each fixture switches between two commits that add, modify and delete files, remove a whole
//! directory, and (in the mixed fixture) change symlinks, executables and gitlinks. The cancel is
//! raised at many points of the apply, from the first file to the last.

use grit_lib::worktree::WorktreeError;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::reset::{ResetMode, ResetOptions};
use grit_lib::porcelain::switch::{SwitchMode, SwitchOptions};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::progress::{CancelToken, NullProgress, ProgressPhase, ProgressSink};
use grit_lib::repo::Repository;

/// Raises `cancel` on the `limit`-th checkout progress increment, after running `at_cancel`.
struct CancelAfter<F: FnMut()> {
    cancel: CancelToken,
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
    open(copy.path())
        .checkout()
        .switch(&detach_to(fx.next), &mut count, &CancelToken::never())
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
        let cancel = CancelToken::new();
        let mut progress = CancelAfter {
            cancel: cancel.clone(),
            limit,
            seen: 0,
            in_checkout: false,
            at_cancel: || {},
        };
        let err = open(&fx.root)
            .checkout()
            .switch(&detach_to(fx.next), &mut progress, &cancel)
            .expect_err(&context);
        assert!(matches!(err, Error::Cancelled), "{context}: {err:?}");
        assert!(progress.seen >= limit, "{context}: cancel never fired");
        assert_eq!(
            repository_state(&fx.root),
            before,
            "{context}: HEAD or index moved"
        );
        assert_main_layout(fx.root, shape, &context);
        assert_clean(fx.root, &context);
    }

    let outcome = open(&fx.root)
        .checkout()
        .switch(
            &detach_to(fx.next),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("retry after cancelled switches");
    assert_eq!(outcome.commit, fx.next);
    assert_eq!(
        git(&fx.root, &["rev-parse", "HEAD"]).trim(),
        fx.next.to_hex()
    );
    assert_clean(fx.root, &format!("{shape:?} retry"));
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
        let cancel = CancelToken::new();
        let mut progress = CancelAfter {
            cancel: cancel.clone(),
            limit,
            seen: 0,
            in_checkout: false,
            at_cancel: || {},
        };
        let err = open(&fx.root)
            .checkout()
            .checkout(Some(&from), &to, &mut progress, &cancel)
            .expect_err(&context);
        assert!(matches!(err, Error::Cancelled), "{context}: {err:?}");
        assert_eq!(repository_state(&fx.root), before, "{context}");
        assert_main_layout(fx.root, Shape::Mixed, &context);
        assert_clean(fx.root, &context);
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
    let cancel = CancelToken::new();
    let mut progress = CancelAfter {
        cancel: cancel.clone(),
        limit: 30,
        seen: 0,
        in_checkout: false,
        at_cancel: || {},
    };
    let err = open(&target)
        .checkout()
        .checkout(None, &tree, &mut progress, &cancel)
        .expect_err("cancelled initial checkout");
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    let left: Vec<_> = fs::read_dir(&target)
        .expect("read clone")
        .map(|e| e.expect("entry").file_name())
        .filter(|name| name != ".git")
        .collect();
    assert!(left.is_empty(), "initial checkout left {left:?}");
    assert!(!target.join(".git/index").exists(), "index written");

    open(&target)
        .checkout()
        .checkout(None, &tree, &mut NullProgress, &CancelToken::never())
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
    let cancel = CancelToken::new();
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
    let err = open(&fx.root)
        .checkout()
        .switch(&detach_to(fx.next), &mut progress, &cancel)
        .expect_err("cancelled switch");
    if locked.is_dir() {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("unlock");
    }
    let Error::Worktree(WorktreeError::CheckoutRollbackIncomplete { source, unrestored }) = &err
    else {
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

/// A cancelled `reset --hard` keeps what it was about to discard: dirty edits it had started
/// to overwrite and a file only the index tracked are back exactly as they were.
#[test]
fn cancelled_hard_reset_keeps_dirty_and_index_only_files() {
    let fx = fixture(Shape::RegularFiles);
    for i in 0..20 {
        write(
            &fx.root,
            &format!("keep/f{i:02}.txt"),
            &format!("dirty edit {i}, longer\n"),
        );
    }
    write(&fx.root, "staged-only.txt", "staged\n");
    git(&fx.root, &["add", "staged-only.txt"]);
    let status_before = git(
        &fx.root,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    let before = repository_state(&fx.root);

    let cancel = CancelToken::new();
    let mut progress = CancelAfter {
        cancel: cancel.clone(),
        limit: 5,
        seen: 0,
        in_checkout: false,
        at_cancel: || {},
    };
    let signature = "T <t@t> 0 +0000".parse().expect("signature");
    let err = reset_to_commit(
        &open(&fx.root),
        &ResetOptions::new(fx.main, ResetMode::Hard, signature, "reset"),
        &mut progress,
        &cancel,
    )
    .expect_err("cancelled reset");
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(progress.seen >= 5, "cancel never fired");
    assert_eq!(repository_state(&fx.root), before, "HEAD or index moved");
    for i in 0..20 {
        assert_eq!(
            fs::read_to_string(fx.root.join(format!("keep/f{i:02}.txt"))).expect("dirty file"),
            format!("dirty edit {i}, longer\n"),
            "dirty edit {i} lost"
        );
    }
    assert_eq!(
        fs::read_to_string(fx.root.join("staged-only.txt"))
            .ok()
            .as_deref(),
        Some("staged\n")
    );
    assert_eq!(
        git(
            &fx.root,
            &["status", "--porcelain", "--untracked-files=all"]
        ),
        status_before
    );
    git(&fx.root, &["fsck", "--no-progress", "--no-dangling"]);
}

/// A cancelled `reset --hard` to a different tree keeps unstaged edits in paths the tree diff
/// replaces or deletes, including files under a directory the reset removes wholesale.
#[test]
fn cancelled_cross_tree_hard_reset_keeps_dirty_tree_diff_paths() {
    let fx = fixture(Shape::RegularFiles);
    let mut dirty = Vec::new();
    for i in 0..60 {
        dirty.push(format!("keep/f{i:02}.txt"));
    }
    for i in (0..20).step_by(2) {
        dirty.push(format!("mixed/m{i:02}.txt"));
    }
    dirty.push("gone/g00.txt".to_owned());
    dirty.push("gone/nested/deep.txt".to_owned());
    for rel in &dirty {
        write(
            &fx.root,
            rel,
            &format!("unstaged edit of {rel}, longer than before\n"),
        );
    }
    let status_before = git(
        &fx.root,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    let signature: grit_lib::ident::Signature = "T <t@t> 0 +0000".parse().expect("signature");

    for limit in [1, 5, 40, 100] {
        let context = format!("cross-tree reset cancel at {limit}");
        let before = repository_state(&fx.root);
        let cancel = CancelToken::new();
        let mut progress = CancelAfter {
            cancel: cancel.clone(),
            limit,
            seen: 0,
            in_checkout: false,
            at_cancel: || {},
        };
        let err = reset_to_commit(
            &open(&fx.root),
            &ResetOptions::new(fx.next, ResetMode::Hard, signature.clone(), "reset"),
            &mut progress,
            &cancel,
        )
        .expect_err(&context);
        assert!(matches!(err, Error::Cancelled), "{context}: {err:?}");
        assert!(progress.seen >= limit, "{context}: cancel never fired");
        assert_eq!(
            repository_state(&fx.root),
            before,
            "{context}: HEAD or index moved"
        );
        let lost: Vec<&String> = dirty
            .iter()
            .filter(|rel| {
                fs::read_to_string(fx.root.join(rel)).ok()
                    != Some(format!("unstaged edit of {rel}, longer than before\n"))
            })
            .collect();
        assert!(
            lost.is_empty(),
            "{context}: unstaged edits lost in {lost:?}"
        );
        assert!(!fx.root.join("new").exists(), "{context}: new/ left behind");
        assert_eq!(
            git(
                &fx.root,
                &["status", "--porcelain", "--untracked-files=all"]
            ),
            status_before,
            "{context}"
        );
    }

    reset_to_commit(
        &open(&fx.root),
        &ResetOptions::new(fx.next, ResetMode::Hard, signature, "reset"),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("uncancelled reset");
    assert_clean(fx.root, "after reset --hard next");
    assert_eq!(
        git(&fx.root, &["rev-parse", "HEAD"]).trim(),
        fx.next.to_hex()
    );
}

/// Same-size edits whose stat still matches a racy index entry (`core.checkStat=minimal`,
/// `core.trustctime=false`, mtimes put back) are real edits to `git status`; a cancelled
/// cross-tree `reset --hard` must keep them too.
#[test]
fn cancelled_hard_reset_keeps_same_size_edits_behind_racy_stat_matches() {
    let fx = fixture(Shape::RegularFiles);
    git(&fx.root, &["config", "core.checkStat", "minimal"]);
    git(&fx.root, &["config", "core.trustctime", "false"]);
    let edited: Vec<String> = (0..60).map(|i| format!("keep/f{i:02}.txt")).collect();
    let mut oldest = None;
    for rel in &edited {
        let path = fx.root.join(rel);
        let mtime = fs::metadata(&path).expect(rel).modified().expect("mtime");
        let before = fs::read_to_string(&path).expect(rel);
        let after = before.replacen("keep", "KEEP", 1);
        assert_eq!(before.len(), after.len());
        fs::write(&path, &after).expect(rel);
        fs::File::options()
            .write(true)
            .open(&path)
            .expect(rel)
            .set_modified(mtime)
            .expect("restore mtime");
        oldest = Some(oldest.map_or(mtime, |o: std::time::SystemTime| o.min(mtime)));
    }
    fs::File::options()
        .write(true)
        .open(fx.root.join(".git/index"))
        .expect("index")
        .set_modified(oldest.expect("edited"))
        .expect("index mtime");
    let status_before = git(
        &fx.root,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    for rel in &edited {
        assert!(
            status_before.contains(&format!(" M {rel}")),
            "git must see the edit of {rel}:\n{status_before}"
        );
    }
    let signature: grit_lib::ident::Signature = "T <t@t> 0 +0000".parse().expect("signature");

    for limit in [5, 100] {
        let context = format!("racy same-size reset cancel at {limit}");
        let before = repository_state(&fx.root);
        let cancel = CancelToken::new();
        let mut progress = CancelAfter {
            cancel: cancel.clone(),
            limit,
            seen: 0,
            in_checkout: false,
            at_cancel: || {},
        };
        let err = reset_to_commit(
            &open(&fx.root),
            &ResetOptions::new(fx.next, ResetMode::Hard, signature.clone(), "reset"),
            &mut progress,
            &cancel,
        )
        .expect_err(&context);
        assert!(matches!(err, Error::Cancelled), "{context}: {err:?}");
        assert!(progress.seen >= limit, "{context}: cancel never fired");
        assert_eq!(
            repository_state(&fx.root),
            before,
            "{context}: HEAD or index moved"
        );
        let lost: Vec<&String> = edited
            .iter()
            .filter(|rel| {
                !fs::read_to_string(fx.root.join(rel))
                    .expect("edited file")
                    .starts_with("KEEP")
            })
            .collect();
        assert!(
            lost.is_empty(),
            "{context}: same-size edits lost in {lost:?}"
        );
        assert_eq!(
            git(
                &fx.root,
                &["status", "--porcelain", "--untracked-files=all"]
            ),
            status_before,
            "{context}"
        );
    }
}

fn wholesale_rename_fixture(file_count: usize) -> (tempfile::TempDir, PathBuf, ObjectId, ObjectId) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    for dir_name in ["olda", "oldb"] {
        for i in 0..file_count {
            write(
                &root,
                &format!("{dir_name}/f{i:03}.txt"),
                &format!("{dir_name} {i}\n"),
            );
        }
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "main"]);
    let main_oid = ObjectId::from_hex(git(&root, &["rev-parse", "HEAD"]).trim()).expect("main");
    git(&root, &["branch", "ren"]);
    git(&root, &["checkout", "-q", "ren"]);
    for dir_name in ["olda", "oldb"] {
        for i in 0..file_count {
            let path = root.join(format!("{dir_name}/f{i:03}.txt"));
            fs::remove_file(&path).expect("remove");
        }
        fs::remove_dir(root.join(dir_name)).expect("rmdir");
    }
    for dir_name in ["newa", "newb"] {
        for i in 0..file_count {
            write(
                &root,
                &format!("{dir_name}/f{i:03}.txt"),
                &format!("{dir_name} {i}\n"),
            );
        }
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "ren"]);
    let ren_oid =
        ObjectId::from_hex(git(&root, &["rev-parse", "refs/heads/ren"]).trim()).expect("ren");
    git(&root, &["checkout", "-q", "main"]);
    assert!(root.join("olda").is_dir(), "fixture: main should have olda");
    assert!(root.join("oldb").is_dir(), "fixture: main should have oldb");
    (dir, root, main_oid, ren_oid)
}

#[test]
fn preset_cancel_switch_returns_before_wholesale_delete() {
    use std::time::{Duration, Instant};

    let (_dir, root, _main, ren) = wholesale_rename_fixture(40);
    let before = repository_state(&root);
    let cancel = CancelToken::new();
    cancel.cancel();
    let start = Instant::now();
    let err = open(&root)
        .checkout()
        .switch(
            &SwitchOptions {
                mode: SwitchMode::Detached {
                    commit: ren,
                    reflog_to_label: "ren".into(),
                },
                dry_run: false,
                hooks: grit_lib::command_runner::hooks::PostOperationHooks::Skip,
            },
            &mut NullProgress,
            &cancel,
        )
        .expect_err("pre-set cancel");
    let elapsed = start.elapsed();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(
        elapsed < Duration::from_millis(800),
        "pre-set cancel took {elapsed:?}"
    );
    assert_eq!(repository_state(&root), before);
    assert!(root.join("olda").is_dir(), "olda removed");
    assert!(root.join("oldb").is_dir(), "oldb removed");
    assert!(!root.join("newa").exists(), "partial apply");
    assert!(!root.join(".git/index.lock").exists(), "index.lock left");
}

#[test]
fn delayed_cancel_switch_during_wholesale_delete_leaves_some_dirs() {
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    let (_dir, root, _main, ren) = wholesale_rename_fixture(120);
    let before = repository_state(&root);
    let cancel = Arc::new(CancelToken::new());
    let flip = cancel.clone();
    let sleeper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(2));
        flip.cancel();
    });
    let err = open(&root)
        .checkout()
        .switch(
            &SwitchOptions {
                mode: SwitchMode::Detached {
                    commit: ren,
                    reflog_to_label: "ren".into(),
                },
                dry_run: false,
                hooks: grit_lib::command_runner::hooks::PostOperationHooks::Skip,
            },
            &mut NullProgress,
            cancel.as_ref(),
        )
        .expect_err("delayed cancel");
    sleeper.join().expect("join");
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert_eq!(repository_state(&root), before);
    assert!(
        root.join("olda").is_dir() || root.join("oldb").is_dir(),
        "both wholesale dirs removed before cancel"
    );
    assert!(!root.join(".git/index.lock").exists());
}
