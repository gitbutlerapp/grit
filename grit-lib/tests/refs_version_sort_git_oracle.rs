//! `compare_refname_versions` must match `git for-each-ref --sort=version:refname`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use grit_lib::refs::version_sort::compare_refname_versions;

fn git_version_order(names: &[&str], suffix: Option<&str>) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("init");
    Command::new("git")
        .args(["commit", "--allow-empty", "-q", "-m", "seed"])
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("commit");
    if let Some(sfx) = suffix {
        Command::new("git")
            .args(["config", "versionsort.suffix", sfx])
            .current_dir(dir.path())
            .status()
            .expect("config");
    }
    for name in names {
        Command::new("git")
            .args(["tag", name])
            .current_dir(dir.path())
            .status()
            .expect("tag");
    }
    let out = Command::new("git")
        .args([
            "for-each-ref",
            "--sort=version:refname",
            "--format=%(refname)",
            "refs/tags/",
        ])
        .current_dir(dir.path())
        .output()
        .expect("for-each-ref");
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

fn grit_version_order(names: &[&str], suffixes: &[String]) -> Vec<String> {
    let mut full: Vec<String> = names.iter().map(|n| format!("refs/tags/{n}")).collect();
    full.sort_by(|a, b| compare_refname_versions(a, b, suffixes));
    full
}

#[test]
fn version_sort_matches_git_tag_table() {
    let names = [
        "1.9",
        "1.10",
        "01.10",
        "v1.0",
        "v1.0-rc1",
        "v1.0-rc2",
        "foo1.3",
        "foo1.6",
        "foo1.10",
        "leading-zero-007",
        "release-rc1",
    ];
    let git = git_version_order(&names, Some("-rc"));
    let grit = grit_version_order(&names, &["-rc".to_owned()]);
    assert_eq!(grit, git, "order mismatch");
}

#[test]
fn version_sort_utf8_tag_names_match_git() {
    let names = ["vα1", "vα2", "vα10"];
    let git = git_version_order(&names, None);
    let grit = grit_version_order(&names, &[]);
    assert_eq!(grit, git);
    let suffixes = vec!["x".to_owned()];
    let _ = compare_refname_versions("refs/tags/vα1", "refs/tags/vα2", &suffixes);
}
