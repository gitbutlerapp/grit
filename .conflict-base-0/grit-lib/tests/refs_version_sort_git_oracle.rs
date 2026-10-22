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
fn version_sort_empty_suffix_matches_git() {
    let names = ["v1.0-rc1", "v1.0", "v1.0-beta1"];
    // Git multi-valued config: -rc, (empty), -beta
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
    for sfx in ["-rc", "", "-beta"] {
        Command::new("git")
            .args(["config", "--add", "versionsort.suffix", sfx])
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
    assert!(out.status.success());
    let git: Vec<String> = out
        .stdout
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect();
    let grit = grit_version_order(
        &names,
        &["-rc".to_owned(), String::new(), "-beta".to_owned()],
    );
    assert_eq!(grit, git);
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

fn generated_version_sort_names() -> Vec<String> {
    let prefixes = ["", "v", "foo", "release-", "refs/tags/a", "pkg-"];
    let cores = [
        "0", "1", "2", "9", "10", "01", "007", "1.3", "1.6", "1.9", "1.10", "01.10", "80", "90",
        "100", "2.0", "2.0.0", "10.0", "rc1", "rc2", "beta1", "alpha",
    ];
    let mut names = Vec::new();
    for p in prefixes {
        for c in cores {
            names.push(format!("{p}{c}"));
            names.push(format!("{p}{c}-rc1"));
            names.push(format!("{p}{c}.patch"));
        }
    }
    names.sort();
    names.dedup();
    names
}

#[test]
fn version_sort_generated_pool_matches_git_and_is_antisymmetric() {
    use std::collections::HashMap;

    let owned = generated_version_sort_names();
    assert!(
        owned.len() > 100,
        "expected a large generated pool, got {}",
        owned.len()
    );
    let names: Vec<&str> = owned.iter().map(String::as_str).collect();

    let git = git_version_order(&names, Some("-rc"));
    let grit = grit_version_order(&names, &["-rc".to_owned()]);
    assert_eq!(grit, git, "full sort order must match git");

    let mut rank: HashMap<String, usize> = HashMap::new();
    for (i, full) in git.iter().enumerate() {
        rank.insert(full.clone(), i);
    }

    for left in &names {
        for right in &names {
            if left == right {
                continue;
            }
            let a = format!("refs/tags/{left}");
            let b = format!("refs/tags/{right}");
            let ab = compare_refname_versions(&a, &b, &["-rc".to_owned()]);
            let ba = compare_refname_versions(&b, &a, &["-rc".to_owned()]);
            assert_eq!(ab, ba.reverse(), "antisymmetry failed for {a} vs {b}");

            let ra = rank[&a];
            let rb = rank[&b];
            let expected = ra.cmp(&rb);
            assert_eq!(ab, expected, "pair {a} vs {b} disagrees with git order");
        }
    }
}
