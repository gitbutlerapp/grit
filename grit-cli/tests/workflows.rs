use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

#[derive(Debug)]
struct CmdOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl CmdOutput {
    fn dump(&self) -> String {
        format!(
            "exit={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.status, self.stdout, self.stderr
        )
    }
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("grit-cli-{tag}-{}-{n}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn gs<I, S>(dir: &Path, args: I) -> Result<CmdOutput, Box<dyn Error>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
    let out = Command::new(GS)
        .args(&args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    Ok(CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn gs_ok<I, S>(dir: &Path, args: I) -> Result<CmdOutput, Box<dyn Error>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = gs(dir, args)?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    Ok(out)
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn path_arg(path: &Path) -> Result<String, Box<dyn Error>> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()).into())
}

fn write_file(path: &Path, contents: &str) -> Result<(), Box<dyn Error>> {
    fs::write(path, contents)?;
    Ok(())
}

fn git_in(dir: &Path, args: &[&str]) -> Result<CmdOutput, Box<dyn Error>> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()?;
    Ok(CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let out = git_in(dir, args)?;
    assert!(out.status == Some(0), "{}", out.dump());
    Ok(())
}

fn git_head(dir: &Path) -> Result<String, Box<dyn Error>> {
    let out = git_in(dir, &["rev-parse", "HEAD"])?;
    assert_eq!(out.status, Some(0));
    Ok(out.stdout.trim().to_owned())
}

/// Nested repository at `parent/sub` with one tracked file; returns its HEAD oid.
fn init_checked_out_submodule(parent: &Path, name: &str) -> Result<String, Box<dyn Error>> {
    let sub = parent.join(name);
    fs::create_dir_all(&sub)?;
    git_ok(&sub, &["init", "-q"])?;
    git_ok(&sub, &["config", "user.name", "Test"])?;
    git_ok(&sub, &["config", "user.email", "t@e.com"])?;
    write_file(&sub.join("tracked.txt"), "v1\n")?;
    git_ok(&sub, &["add", "tracked.txt"])?;
    git_ok(&sub, &["commit", "-qm", "sub init"])?;
    git_head(&sub)
}

#[test]
fn add_pathless_does_not_stage_ignored_files_in_new_directory() -> TestResult {
    let scratch = Scratch::new("add-ignore-dir")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    write_file(&repo.join(".gitignore"), "*.log\n")?;
    write_file(&repo.join("tracked.txt"), "t\n")?;
    gs_ok(&repo, ["add", "tracked.txt"])?;
    gs_ok(&repo, ["commit", "-m", "base"])?;

    fs::create_dir_all(repo.join("new"))?;
    write_file(&repo.join("new/keep.txt"), "keep\n")?;
    write_file(&repo.join("new/ignored.log"), "noise\n")?;

    gs_ok(&repo, ["add"])?;

    git_ok(&repo, &["ls-files", "--stage"])?;
    let listed = git_in(&repo, &["ls-files"])?;
    let files: Vec<_> = listed.stdout.lines().collect();
    assert!(
        files.iter().any(|p| p.ends_with("new/keep.txt")),
        "expected new/keep.txt staged, got: {files:?}"
    );
    assert!(
        !files.iter().any(|p| p.ends_with("ignored.log")),
        "ignored.log must not be staged, got: {files:?}"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn add_explicit_pathspec_does_not_walk_ignored_subtree() -> TestResult {
    use std::os::unix::fs::PermissionsExt;

    let scratch = Scratch::new("add-no-walk")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    write_file(&repo.join(".gitignore"), "blocked/\n")?;
    fs::create_dir_all(repo.join("blocked/secret"))?;
    fs::set_permissions(repo.join("blocked"), fs::Permissions::from_mode(0o000))?;
    write_file(&repo.join("tracked"), "t\n")?;
    gs_ok(&repo, ["add", "tracked"])?;
    Ok(())
}

#[test]
fn add_pathspec_from_subdirectory() -> TestResult {
    let scratch = Scratch::new("add-subdir")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    write_file(&repo.join("a"), "a\n")?;
    gs_ok(&repo, ["commit", "one"])?;

    let deep = repo.join("sub/deep");
    fs::create_dir_all(&deep)?;
    write_file(&deep.join("g"), "g\n")?;

    gs_ok(&deep, ["add", "g"])?;
    let status = gs_ok(&deep, ["status"])?;
    assert!(
        status.stdout.contains("new           g"),
        "expected cwd-relative staged path g:\n{}",
        status.dump()
    );

    gs_ok(&deep, ["add", "g"])?;
    gs_ok(&deep, ["add", "../deep/g"])?;
    gs_ok(&deep, ["add", &path_arg(&deep.join("g"))?])?;
    gs_ok(&deep, ["add", ".."])?;

    let missing = gs(&deep, ["add", "nonexist"])?;
    assert_ne!(missing.status, Some(0), "{}", missing.dump());

    write_file(&deep.join("valid"), "v\n")?;
    let mixed = gs(&deep, ["add", "valid", "missing"])?;
    assert_ne!(mixed.status, Some(0), "{}", mixed.dump());
    let after_fail = gs_ok(&deep, ["status"])?;
    assert!(
        after_fail.stdout.contains("Untracked") && after_fail.stdout.contains("valid"),
        "valid must stay unstaged after partial pathspec failure:\n{}",
        after_fail.dump()
    );

    let sibling = repo.join("d");
    fs::create_dir_all(&sibling)?;
    write_file(&repo.join("a.txt"), "root\n")?;
    write_file(&sibling.join("a.txt"), "nested\n")?;
    gs_ok(&repo, ["add", "a.txt", "d/a.txt"])?;
    let dual = gs_ok(&sibling, ["status"])?;
    assert!(
        dual.stdout.contains("new           ../a.txt"),
        "expected parent-dir display for repo-root file:\n{}",
        dual.dump()
    );
    assert!(
        dual.stdout.contains("new           a.txt"),
        "expected cwd-local display for nested file:\n{}",
        dual.dump()
    );

    Ok(())
}

#[cfg(windows)]
#[test]
fn add_windows_backslash_and_absolute_pathspecs() -> TestResult {
    let scratch = Scratch::new("add-win-pathspec")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    let d = repo.join("d");
    fs::create_dir_all(&d)?;
    write_file(&d.join("a.txt"), "1\n")?;
    write_file(&d.join("b.txt"), "2\n")?;
    write_file(&d.join("c.txt"), "3\n")?;

    gs_ok(&repo, ["add", r"d\a.txt"])?;
    gs_ok(&repo, ["add", &path_arg(&d.join("b.txt"))?])?;
    let forward = d.join("b.txt").to_string_lossy().replace('\\', "/");
    gs_ok(&repo, ["add", &forward])?;

    let status = gs_ok(&d, ["status"])?;
    assert!(
        status.stdout.contains("new           a.txt"),
        "staged file in cwd via backslash pathspec should display as a.txt:\n{}",
        status.dump()
    );
    assert!(
        !status.stdout.contains("new           ../a.txt"),
        "must not show parent-relative path for file in cwd:\n{}",
        status.dump()
    );
    assert!(
        status.stdout.contains("new           b.txt"),
        "staged nested path via absolute pathspec:\n{}",
        status.dump()
    );

    Ok(())
}

#[cfg(unix)]
#[test]
fn status_from_subdirectory_shows_symlink_not_target() -> TestResult {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new("status-symlink")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    let d = repo.join("d");
    fs::create_dir_all(&d)?;
    let outside = scratch.path().join("outside");
    fs::create_dir_all(&outside)?;
    write_file(&outside.join("secret.txt"), "secret\n")?;
    symlink(&outside, d.join("link"))?;
    gs_ok(&repo, ["add", "d/link"])?;
    let status = gs_ok(&d, ["status"])?;
    assert!(
        status.stdout.contains("link"),
        "expected symlink name in status output:\n{}",
        status.dump()
    );
    assert!(
        !status.stdout.contains("outside") && !status.stdout.contains("secret.txt"),
        "must not display symlink target path:\n{}",
        status.dump()
    );

    Ok(())
}

#[test]
fn local_edit_config_commit_status_and_log_workflow() -> TestResult {
    let scratch = Scratch::new("local")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;

    write_file(&repo.join("alpha.txt"), "alpha v1\n")?;
    let status = gs_ok(&repo, std::iter::empty::<&str>())?;
    assert!(status.stdout.contains("On main"));
    assert!(status.stdout.contains("Untracked"));
    assert!(status.stdout.contains("alpha.txt"));

    gs_ok(&repo, ["config", "user.name", "A Developer"])?;
    let name = gs_ok(&repo, ["config", "user.name"])?;
    assert_eq!(name.stdout.trim(), "A Developer");
    let listed = gs_ok(&repo, ["config", "--list"])?;
    assert!(listed.stdout.contains("user.name=A Developer"));
    gs_ok(&repo, ["config", "--unset", "user.name"])?;
    let missing = gs(&repo, ["config", "user.name"])?;
    assert_ne!(missing.status, Some(0), "{}", missing.dump());

    gs_ok(&repo, ["add", "alpha.txt"])?;
    write_file(&repo.join("alpha.txt"), "alpha v2\n")?;
    write_file(&repo.join("beta.txt"), "beta\n")?;
    let committed = gs_ok(&repo, ["commit", "initial commit"])?;
    assert!(committed.stdout.contains("initial commit"));
    assert!(committed.stdout.contains("2 changes committed"));

    let status = gs_ok(&repo, ["status"])?;
    assert!(status.stdout.contains("Nothing to commit"));
    let log = gs_ok(&repo, ["log"])?;
    assert!(log.stdout.contains("initial commit"));
    Ok(())
}

#[test]
fn branch_switch_and_merge_workflow() -> TestResult {
    let scratch = Scratch::new("branch")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    write_file(&repo.join("base.txt"), "base\n")?;
    gs_ok(&repo, ["commit", "base"])?;

    gs_ok(&repo, ["branch", "topic"])?;
    let branches = gs_ok(&repo, ["branch"])?;
    assert!(branches.stdout.contains("* main"));
    assert!(branches.stdout.contains("  topic"));

    gs_ok(&repo, ["switch", "topic"])?;
    write_file(&repo.join("feature.txt"), "feature\n")?;
    gs_ok(&repo, ["commit", "topic work"])?;

    gs_ok(&repo, ["switch", "main"])?;
    assert!(!repo.join("feature.txt").exists());
    write_file(&repo.join("main.txt"), "main\n")?;
    gs_ok(&repo, ["commit", "main work"])?;

    let merge = gs_ok(&repo, ["merge", "topic"])?;
    assert!(merge.stdout.contains("Merged topic"));
    assert_eq!(fs::read_to_string(repo.join("feature.txt"))?, "feature\n");
    assert_eq!(fs::read_to_string(repo.join("main.txt"))?, "main\n");

    let log = gs_ok(&repo, ["log"])?;
    assert!(log.stdout.contains("Merge topic"));
    let status = gs_ok(&repo, ["status"])?;
    assert!(status.stdout.contains("Nothing to commit"));

    let deleted = gs_ok(&repo, ["branch", "-d", "topic"])?;
    assert!(deleted.stdout.contains("(was "));
    let branches = gs_ok(&repo, ["branch"])?;
    assert!(!branches.stdout.contains("topic"));
    Ok(())
}

#[test]
fn branch_delete_refuses_unmerged_and_force_deletes() -> TestResult {
    let scratch = Scratch::new("branch-unmerged")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    write_file(&repo.join("base.txt"), "base\n")?;
    gs_ok(&repo, ["commit", "base"])?;

    gs_ok(&repo, ["switch", "-c", "feat"])?;
    write_file(&repo.join("feat.txt"), "feat\n")?;
    gs_ok(&repo, ["commit", "only on feat"])?;

    gs_ok(&repo, ["switch", "main"])?;
    let refused = gs(&repo, ["branch", "-d", "feat"])?;
    assert_eq!(refused.status, Some(1), "{}", refused.dump());
    assert!(
        refused.stderr.contains("not fully merged"),
        "{}",
        refused.dump()
    );
    let branches = gs_ok(&repo, ["branch"])?;
    assert!(branches.stdout.contains("feat"));

    let deleted = gs_ok(&repo, ["branch", "-D", "feat"])?;
    assert!(deleted.stdout.contains("Deleted branch feat (was "));
    let branches = gs_ok(&repo, ["branch"])?;
    assert!(!branches.stdout.contains("feat"));
    Ok(())
}

#[test]
fn local_remote_clone_push_fetch_and_pull_workflow() -> TestResult {
    let scratch = Scratch::new("remote")?;
    let seed = scratch.child("seed");
    let remote = scratch.child("remote.git");
    let clone = scratch.child("clone");
    fs::create_dir_all(&seed)?;

    gs_ok(&seed, ["init", "."])?;
    write_file(&seed.join("README.md"), "seed\n")?;
    gs_ok(&seed, ["commit", "seed commit"])?;

    gs_ok(
        scratch.path(),
        ["init", "--bare", path_arg(&remote)?.as_str()],
    )?;
    gs_ok(
        &seed,
        ["remote", "add", "origin", path_arg(&remote)?.as_str()],
    )?;
    let pushed = gs_ok(&seed, ["push"])?;
    assert!(pushed.stdout.contains("pushed main"));

    gs_ok(
        scratch.path(),
        [
            "clone",
            path_arg(&remote)?.as_str(),
            path_arg(&clone)?.as_str(),
        ],
    )?;
    assert_eq!(fs::read_to_string(clone.join("README.md"))?, "seed\n");
    let cloned_log = gs_ok(&clone, ["log"])?;
    assert!(cloned_log.stdout.contains("seed commit"));

    write_file(&clone.join("clone.txt"), "from clone\n")?;
    gs_ok(&clone, ["commit", "-am", "clone work"])?;
    gs_ok(&clone, ["push"])?;

    let status_after_push = gs_ok(&clone, ["status"])?;
    assert!(
        status_after_push.stdout.contains("even with origin/main"),
        "status after push should reflect updated remote-tracking ref:\n{}",
        status_after_push.dump()
    );

    let fetched = gs_ok(&seed, ["fetch"])?;
    assert!(fetched.stdout.contains("Fetched"));
    let pulled = gs_ok(&seed, ["pull"])?;
    assert!(
        pulled.stdout.contains("Fast-forwarded")
            || pulled.stdout.contains("Already up to date")
            || pulled.stdout.contains("Merged")
    );
    assert_eq!(fs::read_to_string(seed.join("clone.txt"))?, "from clone\n");
    Ok(())
}

/// GitHub issue #909: relative local clone URLs must be stored absolute and push must reach
/// the real remote, not create a bare repo inside the worktree.
#[test]
fn relative_local_clone_push_reaches_real_remote() -> TestResult {
    let scratch = Scratch::new("rel-clone-909")?;
    let remote = scratch.child("origin.git");
    let clone = scratch.child("c");
    let seed = scratch.child("seed");
    fs::create_dir_all(&seed)?;

    gs_ok(
        scratch.path(),
        ["init", "--bare", path_arg(&remote)?.as_str()],
    )?;
    gs_ok(&seed, ["init", "."])?;
    write_file(&seed.join("f"), "1\n")?;
    gs_ok(&seed, ["commit", "-m", "seed"])?;
    git_in(
        &seed,
        &["remote", "add", "origin", path_arg(&remote)?.as_str()],
    )?;
    git_in(&seed, &["push", "-q", "origin", "HEAD:refs/heads/main"])?;

    gs_ok(
        scratch.path(),
        ["clone", "origin.git", path_arg(&clone)?.as_str()],
    )?;

    let remote_abs = path_arg(&remote.canonicalize().unwrap_or(remote.clone()))?;
    let remotes = gs_ok(&clone, ["remote"])?;
    assert!(
        remotes.stdout.contains(&format!("origin\t{remote_abs}")),
        "origin URL should be absolute path to real remote, got:\n{}",
        remotes.dump()
    );

    write_file(&clone.join("g"), "x\n")?;
    gs_ok(&clone, ["commit", "-am", "two"])?;
    gs_ok(&clone, ["push"])?;

    assert!(
        !clone.join("origin.git").exists(),
        "push must not create origin.git inside the clone worktree"
    );

    let remote_log = git_in(&remote, &["log", "--oneline", "main"])?;
    assert_eq!(remote_log.status, Some(0));
    assert!(
        remote_log.stdout.contains("two"),
        "real remote should have commit 'two':\n{}",
        remote_log.stdout
    );
    Ok(())
}

#[test]
fn file_url_clone_preserves_url_and_push_succeeds() -> TestResult {
    let scratch = Scratch::new("file-url-clone")?;
    let remote = scratch.child("origin.git");
    let clone = scratch.child("dst");
    let seed = scratch.child("seed");
    fs::create_dir_all(&seed)?;

    gs_ok(
        scratch.path(),
        ["init", "--bare", path_arg(&remote)?.as_str()],
    )?;
    gs_ok(&seed, ["init", "."])?;
    write_file(&seed.join("f"), "1\n")?;
    gs_ok(&seed, ["commit", "-m", "seed"])?;
    git_in(
        &seed,
        &["remote", "add", "origin", path_arg(&remote)?.as_str()],
    )?;
    git_in(&seed, &["push", "-q", "origin", "HEAD:refs/heads/main"])?;

    let remote_abs = remote.canonicalize().unwrap_or(remote.clone());
    let file_url = format!("file://{}", path_arg(&remote_abs)?);
    gs_ok(
        scratch.path(),
        ["clone", file_url.as_str(), path_arg(&clone)?.as_str()],
    )?;

    let remotes = gs_ok(&clone, ["remote"])?;
    assert!(
        remotes.stdout.contains(&format!("origin\t{file_url}")),
        "file:// URL must be preserved in config:\n{}",
        remotes.dump()
    );

    write_file(&clone.join("g"), "x\n")?;
    gs_ok(&clone, ["commit", "-am", "two"])?;
    gs_ok(&clone, ["push"])?;

    let remote_log = git_in(&remote, &["log", "--oneline", "main"])?;
    assert!(
        remote_log.stdout.contains("two"),
        "push via file:// remote should update real origin:\n{}",
        remote_log.stdout
    );
    Ok(())
}

/// Git treats `?` as a literal path character in `file://` URLs (`%3F` must not become a query).
#[test]
fn file_url_percent_question_mark_clone_and_fetch() -> TestResult {
    let scratch = Scratch::new("file-qmark-clone")?;
    let remote = scratch.child("origin?repo.git");
    let clone = scratch.child("dst");
    let seed = scratch.child("seed");
    fs::create_dir_all(&seed)?;

    gs_ok(
        scratch.path(),
        ["init", "--bare", path_arg(&remote)?.as_str()],
    )?;
    gs_ok(&seed, ["init", "."])?;
    write_file(&seed.join("f"), "1\n")?;
    gs_ok(&seed, ["commit", "-m", "seed"])?;
    git_in(
        &seed,
        &["remote", "add", "origin", path_arg(&remote)?.as_str()],
    )?;
    git_in(&seed, &["push", "-q", "origin", "HEAD:refs/heads/main"])?;

    let remote_abs = remote.canonicalize().unwrap_or(remote.clone());
    let path = path_arg(&remote_abs)?;
    let file_url = format!("file://{}", path.replace('?', "%3F"));
    gs_ok(
        scratch.path(),
        ["clone", file_url.as_str(), path_arg(&clone)?.as_str()],
    )?;
    gs_ok(&clone, ["fetch", "origin"])?;
    Ok(())
}

/// `file://localhost` must resolve as an absolute path (Git-compatible), not under the clone dir.
#[test]
fn file_localhost_url_clone_and_fetch() -> TestResult {
    let scratch = Scratch::new("file-localhost-clone")?;
    let remote = scratch.child("origin.git");
    let clone = scratch.child("dst");
    let seed = scratch.child("seed");
    fs::create_dir_all(&seed)?;

    gs_ok(
        scratch.path(),
        ["init", "--bare", path_arg(&remote)?.as_str()],
    )?;
    gs_ok(&seed, ["init", "."])?;
    write_file(&seed.join("f"), "1\n")?;
    gs_ok(&seed, ["commit", "-m", "seed"])?;
    git_in(
        &seed,
        &["remote", "add", "origin", path_arg(&remote)?.as_str()],
    )?;
    git_in(&seed, &["push", "-q", "origin", "HEAD:refs/heads/main"])?;

    let remote_abs = remote.canonicalize().unwrap_or(remote.clone());
    let file_url = format!("file://localhost{}", path_arg(&remote_abs)?);
    gs_ok(
        scratch.path(),
        ["clone", file_url.as_str(), path_arg(&clone)?.as_str()],
    )?;

    gs_ok(&clone, ["fetch", "origin"])?;

    let remotes = gs_ok(&clone, ["remote"])?;
    assert!(
        remotes.stdout.contains(&format!("origin\t{file_url}")),
        "file://localhost URL must be preserved in config:\n{}",
        remotes.dump()
    );
    Ok(())
}

/// Mirrors GitHub issue #903: a gitlink whose commit object is not in the superproject ODB
/// must not make `grit diff` fail on a clean tree.
#[test]
fn diff_succeeds_with_uninitialized_gitlink_on_clean_tree() -> TestResult {
    let scratch = Scratch::new("gitlink-diff")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    gs_ok(&repo, ["config", "commit.gpgsign", "false"])?;
    write_file(&repo.join("a"), "a\n")?;
    fs::create_dir(&repo.join("sub"))?;

    let null = null_device();
    let gitlink_oid = "855827c583bc30645ba427885caa40c5b81764d2";
    let add_a = Command::new("git")
        .current_dir(&repo)
        .args(["add", "a"])
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_CONFIG_SYSTEM", null)
        .output()?;
    assert!(add_a.status.success(), "git add failed");
    let cacheinfo = Command::new("git")
        .current_dir(&repo)
        .args([
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{gitlink_oid},sub"),
        ])
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_CONFIG_SYSTEM", null)
        .output()?;
    assert!(cacheinfo.status.success(), "git update-index failed");
    gs_ok(&repo, ["commit", "-m", "init"])?;

    let diff = gs(&repo, ["diff"])?;
    assert_eq!(diff.status, Some(0), "{}", diff.dump());
    assert!(
        !diff.stderr.contains("object not found"),
        "gitlink must not trigger ODB blob read:\n{}",
        diff.dump()
    );

    write_file(&repo.join("a"), "a changed\n")?;
    let diff = gs(&repo, ["diff"])?;
    assert_eq!(diff.status, Some(0), "{}", diff.dump());
    assert!(
        diff.stdout.contains("a changed") || diff.stdout.contains("changed"),
        "ordinary file edits should still appear:\n{}",
        diff.dump()
    );
    Ok(())
}

#[test]
fn diff_commit_gitlink_to_regular_file_typechange() -> TestResult {
    let scratch = Scratch::new("gitlink-to-file")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    gs_ok(&repo, ["config", "commit.gpgsign", "false"])?;

    let sub_oid = init_checked_out_submodule(&repo, "sub")?;
    git_ok(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_oid},sub"),
        ],
    )?;
    gs_ok(&repo, ["commit", "-m", "gitlink"])?;

    fs::remove_dir_all(repo.join("sub"))?;
    write_file(&repo.join("sub"), "plain file\n")?;
    gs_ok(&repo, ["add", "sub"])?;
    gs_ok(&repo, ["commit", "-m", "replace with file"])?;

    let diff = gs(&repo, ["diff", "HEAD"])?;
    assert_eq!(diff.status, Some(0), "{}", diff.dump());
    assert!(
        diff.stdout.contains("Subproject commit"),
        "gitlink side should remain a subproject line:\n{}",
        diff.dump()
    );
    assert!(
        diff.stdout.contains("plain file"),
        "regular file content must appear on the new side:\n{}",
        diff.dump()
    );
    Ok(())
}

#[test]
fn diff_commit_regular_file_to_gitlink_typechange() -> TestResult {
    let scratch = Scratch::new("file-to-gitlink")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    gs_ok(&repo, ["config", "commit.gpgsign", "false"])?;

    write_file(&repo.join("sub"), "plain file\n")?;
    gs_ok(&repo, ["add", "sub"])?;
    gs_ok(&repo, ["commit", "-m", "file"])?;

    fs::remove_file(repo.join("sub"))?;
    let sub_oid = init_checked_out_submodule(&repo, "sub")?;
    git_ok(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_oid},sub"),
        ],
    )?;
    gs_ok(&repo, ["commit", "-m", "replace with gitlink"])?;

    let diff = gs(&repo, ["diff", "HEAD"])?;
    assert_eq!(diff.status, Some(0), "{}", diff.dump());
    assert!(
        diff.stdout.contains("plain file"),
        "removed regular file content must appear:\n{}",
        diff.dump()
    );
    assert!(
        diff.stdout.contains("Subproject commit"),
        "new gitlink side must be a subproject line:\n{}",
        diff.dump()
    );
    Ok(())
}

#[test]
fn diff_dirty_initialized_submodule_shows_dirty_suffix() -> TestResult {
    let scratch = Scratch::new("gitlink-dirty")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    gs_ok(&repo, ["config", "commit.gpgsign", "false"])?;

    let sub_oid = init_checked_out_submodule(&repo, "sub")?;
    git_ok(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_oid},sub"),
        ],
    )?;
    gs_ok(&repo, ["commit", "-m", "gitlink"])?;

    write_file(&repo.join("sub/tracked.txt"), "dirty worktree\n")?;
    let diff = gs(&repo, ["diff"])?;
    assert_eq!(diff.status, Some(0), "{}", diff.dump());
    assert!(
        diff.stdout.contains("-dirty"),
        "dirty submodule worktree should suffix the plus line like Git:\n{}",
        diff.dump()
    );
    assert!(
        diff.stdout.contains("Subproject commit"),
        "expected subproject hunk:\n{}",
        diff.dump()
    );
    Ok(())
}

#[test]
fn diff_untracked_only_inside_submodule_is_empty() -> TestResult {
    let scratch = Scratch::new("gitlink-untracked-only")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    gs_ok(&repo, ["config", "commit.gpgsign", "false"])?;

    let sub_oid = init_checked_out_submodule(&repo, "sub")?;
    git_ok(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_oid},sub"),
        ],
    )?;
    gs_ok(&repo, ["commit", "-m", "gitlink"])?;

    write_file(&repo.join("sub/untracked.txt"), "not in sub index\n")?;
    let diff = gs(&repo, ["diff"])?;
    assert_eq!(diff.status, Some(0), "{}", diff.dump());
    assert!(
        !diff.stdout.contains("Subproject commit"),
        "untracked-only submodule content must not produce a gitlink hunk:\n{}",
        diff.dump()
    );
    Ok(())
}

#[test]
fn diff_moved_submodule_head_with_tracked_dirt_shows_dirty_suffix() -> TestResult {
    let scratch = Scratch::new("gitlink-moved-dirty")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, ["init", "."])?;
    gs_ok(&repo, ["config", "commit.gpgsign", "false"])?;

    let sub_oid_a = init_checked_out_submodule(&repo, "sub")?;
    git_ok(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_oid_a},sub"),
        ],
    )?;
    gs_ok(&repo, ["commit", "-m", "gitlink at A"])?;

    let sub = repo.join("sub");
    write_file(&sub.join("tracked.txt"), "v2\n")?;
    git_ok(&sub, &["add", "tracked.txt"])?;
    git_ok(&sub, &["commit", "-qm", "advance to B"])?;
    let sub_oid_b = git_head(&sub)?;
    assert_ne!(sub_oid_a, sub_oid_b);

    write_file(&sub.join("tracked.txt"), "v2 dirty\n")?;
    let diff = gs(&repo, ["diff"])?;
    assert_eq!(diff.status, Some(0), "{}", diff.dump());
    assert!(
        diff.stdout.contains("-dirty"),
        "tracked dirt with advanced HEAD should suffix the plus line:\n{}",
        diff.dump()
    );
    assert!(
        diff.stdout.contains(&sub_oid_b),
        "plus line should use the checked-out HEAD commit:\n{}",
        diff.dump()
    );
    Ok(())
}
