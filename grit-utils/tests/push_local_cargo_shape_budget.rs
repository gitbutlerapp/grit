//! Gate for issue #2493: local `grit push` into an empty bare repo must stay
//! within ~1.3× git's peak RSS on a fixture that forces a pack rebuild (objects
//! in the source pack that are not on the pushed branch).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use grit_utils::repack_fixture::{build_cargo_shape_repo, CargoShape};
use tempfile::TempDir;

const TIME_SLACK: Duration = Duration::from_millis(500);
const RSS_SLACK: u64 = 8 * 1024 * 1024;

fn resolve_git() -> PathBuf {
    grit_utils::binary::resolve_binary("git", None).expect("git")
}

fn build_grit() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let release = !cfg!(debug_assertions);
    let mut args = vec!["build", "-p", "grit-cli", "--bin", "grit", "-q"];
    if release {
        args.push("--release");
    }
    let status = Command::new(env!("CARGO"))
        .args(&args)
        .current_dir(&workspace)
        .status()
        .expect("cargo build grit");
    assert!(status.success());
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    target
        .join(if release { "release" } else { "debug" })
        .join(format!("grit{}", std::env::consts::EXE_SUFFIX))
}

fn measure(cwd: &Path, command: &str) -> (u64, Duration) {
    let start = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_grit-bench"))
        .arg("measure-rss")
        .arg("--cwd")
        .arg(cwd)
        .env("GRIT_BENCH_MEASURE_CMD", command)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("measure-rss");
    let elapsed = start.elapsed();
    assert!(
        out.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rss = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("rss");
    (rss, elapsed)
}

fn git(git: &Path, dir: &Path, args: &[&str]) -> String {
    let out = Command::new(git)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn quote(path: &Path) -> String {
    grit_utils::shell::shell_quote(path.to_str().expect("utf-8 path"))
}

/// One shared pack holds `main` and unreachable `side` objects; push `main` only.
fn build_side_branch_fixture(git: &Path, root: &Path) {
    std::fs::create_dir_all(root).expect("mkdir");
    git(git, root, &["init", "-q", "-b", "main"]);
    for i in 0..60 {
        let name = format!("main{i}.txt");
        std::fs::write(root.join(&name), format!("main payload {i}\n")).expect("write");
        git(git, root, &["add", &name]);
        git(
            git,
            root,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                &format!("main {i}"),
            ],
        );
    }
    git(git, root, &["branch", "side", "HEAD~15"]);
    git(git, root, &["checkout", "-q", "side"]);
    for i in 0..30 {
        let name = format!("side{i}.txt");
        std::fs::write(root.join(&name), format!("side payload {i}\n")).expect("write");
        git(git, root, &["add", &name]);
        git(
            git,
            root,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                &format!("side {i}"),
            ],
        );
    }
    git(git, root, &["checkout", "-q", "main"]);
    git(git, root, &["repack", "-adq"]);
    let side_tip = git(git, root, &["rev-parse", "side"]);
    let (ok, _) = {
        let out = Command::new(git)
            .args(["cat-file", "-e", &format!("{side_tip}^{{commit}}")])
            .current_dir(root)
            .output()
            .expect("cat-file");
        (out.status.success(), ())
    };
    assert!(ok, "side tip must exist in the repacked store");
    let (reachable, _) = {
        let main_tip = git(git, root, &["rev-parse", "main"]);
        let out = Command::new(git)
            .args(["merge-base", "--is-ancestor", &side_tip, &main_tip])
            .current_dir(root)
            .output()
            .expect("merge-base");
        (out.status.success(), ())
    };
    assert!(
        !reachable,
        "side tip must not be reachable from main (forces pack rebuild on push)"
    );
}

fn check_local_push_budgets(
    label: &str,
    git_bin: &Path,
    grit: &Path,
    fixture: &Path,
    dest_grit: &Path,
    dest_git: &Path,
    rss_percent: u64,
) {
    let tip = git(git_bin, fixture, &["rev-parse", "main"]);
    git(git_bin, dest_grit, &["init", "-q", "--bare"]);
    git(git_bin, dest_git, &["init", "-q", "--bare"]);

    let grit_dest = quote(dest_grit);
    let git_dest = quote(dest_git);
    let (grit_base, _) = measure(fixture, &format!("{} log -n 1", quote(grit)));
    let (git_base, _) = measure(fixture, &format!("{} log -1", quote(git_bin)));
    let (git_rss, git_time) = measure(
        fixture,
        &format!("{} push -q {git_dest} main", quote(git_bin)),
    );
    let (grit_rss, grit_time) = measure(fixture, &format!("{} push {grit_dest} main", quote(grit)));

    assert_eq!(git(git_bin, dest_grit, &["rev-parse", "main"]), tip);
    git(git_bin, dest_grit, &["fsck", "--strict", "--no-dangling"]);
    eprintln!(
        "{label}: git {git_time:?} {git_rss} B rss (base {git_base} B); \
         grit {grit_time:?} {grit_rss} B rss (base {grit_base} B)"
    );

    let grit_excess = grit_rss.saturating_sub(grit_base);
    let git_excess = git_rss.saturating_sub(git_base);
    assert!(
        grit_excess * 100 <= git_excess * rss_percent + RSS_SLACK * 100,
        "{label}: grit push peak RSS {grit_rss} B ({grit_excess} B over `log -n 1`) \
         vs git {git_rss} B ({git_excess} B over `log -1`)"
    );
    if !cfg!(debug_assertions) {
        assert!(
            grit_time <= 2 * git_time + TIME_SLACK,
            "{label}: grit push took {grit_time:?} vs git {git_time:?}"
        );
    }
}

#[test]
fn push_local_rebuild_stays_within_git_rss_on_side_branch_fixture() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let git_bin = resolve_git();
    let grit = build_grit();
    let tmp = TempDir::new().expect("tempdir");
    let fixture = tmp.path().join("fixture");
    build_side_branch_fixture(&git_bin, &fixture);
    check_local_push_budgets(
        "side branch repack",
        &git_bin,
        &grit,
        &fixture,
        &tmp.path().join("grit.git"),
        &tmp.path().join("git.git"),
        130,
    );
}

#[test]
fn push_local_stays_within_git_rss_on_cargo_shape() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let git_bin = resolve_git();
    let grit = build_grit();
    let tmp = TempDir::new().expect("tempdir");
    let fixture = tmp.path().join("fixture");
    build_cargo_shape_repo(&git_bin, &fixture, CargoShape::SMOKE).expect("fixture");
    check_local_push_budgets(
        "cargo shape",
        &git_bin,
        &grit,
        &fixture,
        &tmp.path().join("grit.git"),
        &tmp.path().join("git.git"),
        130,
    );
}
