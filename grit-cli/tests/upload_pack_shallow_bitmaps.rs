//! System `git` deepening and unshallowing a shallow clone from `grit upload-pack`
//! when the served repository has reachability bitmaps, over protocol v0 and v2.
//! The client offers its shallow tip as a have, so the server must not trust a
//! bitmap closure of that have. Results are compared to `git upload-pack`.

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

/// Twelve linear commits on `main`, fully repacked with a bitmap.
fn bitmapped_repo(tmp: &Path) -> PathBuf {
    let work = tmp.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main", "."]);
    for i in 1..=12 {
        std::fs::write(work.join(format!("f{}", i % 3)), format!("{i}\n")).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", &format!("c{i}")]);
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
    git(&bare, &["repack", "-adbq"]);
    bare
}

fn sorted_lines(s: &str) -> Vec<String> {
    let mut lines: Vec<String> = s.lines().map(str::to_owned).collect();
    lines.sort();
    lines
}

/// Clone `url` at depth 1, then fetch from it again with `fetch_args` through
/// `upload_pack`; returns the clone.
fn deepened_clone(
    tmp: &Path,
    url: &str,
    upload_pack: &str,
    version: &str,
    fetch_args: &[&str],
    name: &str,
) -> PathBuf {
    let dest = tmp.join(name);
    git(
        tmp,
        &["clone", "-q", "--depth", "1", url, dest.to_str().unwrap()],
    );
    let proto = format!("protocol.version={version}");
    let up = format!("--upload-pack={upload_pack}");
    let mut args = vec!["-c", proto.as_str(), "fetch", "-q", up.as_str()];
    args.extend_from_slice(fetch_args);
    args.push("origin");
    git(&dest, &args);
    dest
}

#[test]
fn git_deepen_and_unshallow_from_grit_on_bitmapped_repo_match_git() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    let bare = bitmapped_repo(tmp);
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let grit_upload_pack = format!("{GRIT} upload-pack");

    let cases: [(&str, &[&str], &str); 2] = [
        ("deepen", &["--deepen", "3"], "4"),
        ("unshallow", &["--unshallow"], "12"),
    ];
    for version in ["0", "2"] {
        for (label, fetch_args, commits) in cases {
            let label = format!("v{version}-{label}");
            let grit = deepened_clone(
                tmp,
                &url,
                &grit_upload_pack,
                version,
                fetch_args,
                &format!("grit-{label}"),
            );
            let reference = deepened_clone(
                tmp,
                &url,
                "git-upload-pack",
                version,
                fetch_args,
                &format!("git-{label}"),
            );
            git(&grit, &["fsck", "--no-dangling"]);
            assert_eq!(
                git(&grit, &["rev-list", "--count", "HEAD"]),
                commits,
                "{label}: commit count"
            );
            assert_eq!(
                sorted_lines(&git(&grit, &["rev-list", "--objects", "--all"])),
                sorted_lines(&git(&reference, &["rev-list", "--objects", "--all"])),
                "{label}: object set"
            );
            let shallow = |dir: &Path| {
                sorted_lines(&std::fs::read_to_string(dir.join(".git/shallow")).unwrap_or_default())
            };
            assert_eq!(shallow(&grit), shallow(&reference), "{label}: shallow file");
        }
    }
}
