//! System `git` cloning and fetching from `grit upload-pack`: `--shallow-exclude`
//! (`deepen-not <ref>`) over protocol v0 and v2, and a v2 fetch by ref name that
//! uses `want-ref` when the server allows it. Results are compared to the same
//! operations served by `git upload-pack`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_cmd(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_DATE", "2005-04-07T22:13:13 +0200")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Six linear commits on `main`; `v3` (lightweight) and `av3` (annotated) tag the third.
fn served_repo(tmp: &Path) -> PathBuf {
    let work = tmp.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    for i in 1..=6 {
        std::fs::write(work.join(format!("f{i}")), format!("{i}\n")).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", &format!("c{i}")]);
        if i == 3 {
            git(&work, &["tag", "v3"]);
            git(&work, &["tag", "-a", "-m", "annotated", "av3"]);
        }
    }
    let bare = tmp.join("srv.git");
    git(
        tmp,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.canonicalize().unwrap().display())
}

fn grit_upload_pack() -> String {
    format!("{GRIT} upload-pack")
}

fn sorted_lines(s: &str) -> Vec<String> {
    let mut lines: Vec<String> = s.lines().map(str::to_owned).collect();
    lines.sort();
    lines
}

fn shallow_file(git_dir: &Path) -> Vec<String> {
    sorted_lines(&std::fs::read_to_string(git_dir.join("shallow")).unwrap_or_default())
}

fn clone_shallow_exclude(
    tmp: &Path,
    url: &str,
    exclude: &str,
    upload_pack: &str,
    version: &str,
    name: &str,
) -> PathBuf {
    let dest = tmp.join(name);
    let up = format!("--upload-pack={upload_pack}");
    let excl = format!("--shallow-exclude={exclude}");
    let proto = format!("protocol.version={version}");
    // Without tag refs, the reachable history is decided by the deepen-not
    // boundary alone, independent of which tags include-tag adds to the pack.
    git(
        tmp,
        &[
            "-c",
            &proto,
            "clone",
            "-q",
            "--bare",
            "--no-tags",
            &excl,
            &up,
            url,
            dest.to_str().unwrap(),
        ],
    );
    dest
}

#[test]
fn git_clone_shallow_exclude_from_grit_matches_git() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    let bare = served_repo(tmp);
    let url = file_url(&bare);
    let boundary = git(&bare, &["rev-parse", "main~2"]);

    for version in ["0", "2"] {
        for exclude in ["v3", "refs/tags/v3", "av3"] {
            let label = format!("v{version}-{}", exclude.replace('/', "_"));
            let grit = clone_shallow_exclude(
                tmp,
                &url,
                exclude,
                &grit_upload_pack(),
                version,
                &format!("grit-{label}"),
            );
            let reference = clone_shallow_exclude(
                tmp,
                &url,
                exclude,
                "git-upload-pack",
                version,
                &format!("git-{label}"),
            );

            assert_eq!(
                git(&grit, &["rev-list", "--count", "--all"]),
                "3",
                "{label}: commit count"
            );
            assert_eq!(
                shallow_file(&grit),
                std::slice::from_ref(&boundary),
                "{label}: shallow file"
            );
            assert_eq!(shallow_file(&grit), shallow_file(&reference), "{label}");
            assert_eq!(
                sorted_lines(&git(&grit, &["rev-list", "--objects", "--all"])),
                sorted_lines(&git(&reference, &["rev-list", "--objects", "--all"])),
                "{label}: object set"
            );
            git(&grit, &["fsck", "--no-dangling"]);
        }
    }
}

#[test]
fn git_clone_shallow_exclude_unknown_ref_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    let bare = served_repo(tmp);
    let dest = tmp.join("clone");
    let out = git_cmd(
        tmp,
        &[
            "clone",
            "-q",
            "--bare",
            "--shallow-exclude=no-such-ref",
            &format!("--upload-pack={}", grit_upload_pack()),
            &file_url(&bare),
            dest.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no-such-ref"), "stderr: {stderr}");
}

#[test]
fn git_fetch_by_name_uses_want_ref_and_updates_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    let bare = served_repo(tmp);
    git(&bare, &["config", "uploadpack.allowRefInWant", "true"]);
    let tip = git(&bare, &["rev-parse", "main"]);

    let client = tmp.join("client");
    std::fs::create_dir_all(&client).unwrap();
    git(&client, &["init", "-q"]);
    let trace = tmp.join("packet.trace");
    let out = Command::new("git")
        .current_dir(&client)
        .args([
            "-c",
            "protocol.version=2",
            "fetch",
            "-q",
            &format!("--upload-pack={}", grit_upload_pack()),
            &file_url(&bare),
            "refs/heads/main:refs/remotes/origin/main",
        ])
        .env("GIT_TRACE_PACKET", &trace)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fetch");
    assert!(
        out.status.success(),
        "git fetch failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let trace = std::fs::read_to_string(&trace).unwrap();
    assert!(
        trace.contains("want-ref refs/heads/main"),
        "client did not use want-ref"
    );
    assert!(
        trace.contains("wanted-refs"),
        "server sent no wanted-refs section"
    );
    assert_eq!(
        git(&client, &["rev-parse", "refs/remotes/origin/main"]),
        tip
    );
    git(&client, &["fsck", "--no-dangling"]);
}
