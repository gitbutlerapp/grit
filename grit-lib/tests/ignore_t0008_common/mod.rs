//! Shared t0008 ignore fixture for integration tests.
//!
//! Several test binaries include this module and each uses only part of it.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::worktree::ignore::IgnoreMatch;
use grit_test_support::{git_cmd, unique_tmp};

pub const AUTHOR: &str = "Ignore Test Author";
pub const EMAIL: &str = "ignore-test@example.com";
pub const DATE: &str = "1700000000 +0000";

pub struct GitCheckLine {
    pub ignored: bool,
    pub source: Option<String>,
    pub line: Option<usize>,
    pub pattern: Option<String>,
}

pub fn hermetic_git(home: &Path, repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", AUTHOR)
        .env("GIT_AUTHOR_EMAIL", EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR)
        .env("GIT_COMMITTER_EMAIL", EMAIL)
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_DATE", DATE)
        .output()
        .expect("spawn git")
}

pub fn parse_git_check_ignore_output(stdout: &str) -> HashMap<String, GitCheckLine> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let (meta, path) = line.split_once('\t').unwrap_or((line, ""));
        let path = path.trim_end_matches('\r').to_owned();
        if meta.starts_with("::") {
            map.insert(
                path,
                GitCheckLine {
                    ignored: false,
                    source: None,
                    line: None,
                    pattern: None,
                },
            );
            continue;
        }
        let parts: Vec<&str> = meta.rsplitn(3, ':').collect();
        if parts.len() != 3 {
            continue;
        }
        let [pattern, line_s, source] = [parts[0], parts[1], parts[2]];
        let negated = pattern.starts_with('!');
        map.insert(
            path,
            GitCheckLine {
                ignored: !negated,
                source: Some(source.to_owned()),
                line: Some(line_s.parse().expect("line number")),
                pattern: Some(pattern.to_owned()),
            },
        );
    }
    map
}

pub fn git_check_ignore_verbose_batch(
    home: &Path,
    repo: &Path,
    paths: &[&str],
) -> HashMap<String, GitCheckLine> {
    let mut args = vec!["check-ignore", "-v", "-n", "--non-matching", "--"];
    args.extend(paths);
    let out = git_cmd(&args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .in_dir(repo)
        .exec();
    let code = out.status.unwrap_or(-1);
    assert!(
        code == 0 || code == 1,
        "git check-ignore batch: {}",
        out.dump("git")
    );
    parse_git_check_ignore_output(&out.stdout)
}

pub fn assert_match_eq(
    path: &str,
    git: &GitCheckLine,
    grit_ignored: bool,
    grit_match: &Option<IgnoreMatch>,
) {
    assert_eq!(
        grit_ignored, git.ignored,
        "ignored mismatch for {path}: grit={grit_ignored} git={}",
        git.ignored
    );
    if git.ignored {
        let m = grit_match.as_ref().expect("grit match metadata");
        let gsrc = git.source.as_ref().expect("git source");
        let grit_src = m.source.to_string();
        assert!(
            grit_src.ends_with(gsrc.trim_start_matches('/'))
                || gsrc.ends_with(&grit_src)
                || grit_src == *gsrc,
            "source mismatch for {path}: grit={grit_src} git={gsrc}"
        );
        assert_eq!(Some(m.line_number), git.line, "line mismatch for {path}");
        assert_eq!(
            Some(m.pattern_text.as_str()),
            git.pattern.as_deref(),
            "pattern mismatch for {path}"
        );
        if let Some(pat) = &git.pattern {
            assert_eq!(m.negative, pat.starts_with('!'), "negation flag for {path}");
        }
    } else if let Some(pat) = &git.pattern {
        let m = grit_match
            .as_ref()
            .expect("grit match metadata for negated non-match");
        assert!(m.negative, "expected negated match metadata for {path}");
        let gsrc = git.source.as_ref().expect("git source for negated match");
        let grit_src = m.source.to_string();
        assert!(
            grit_src.ends_with(gsrc.trim_start_matches('/'))
                || gsrc.ends_with(&grit_src)
                || grit_src == *gsrc,
            "source mismatch for {path}: grit={grit_src} git={gsrc}"
        );
        assert_eq!(Some(m.line_number), git.line, "line mismatch for {path}");
        assert_eq!(
            Some(m.pattern_text.as_str()),
            Some(pat.as_str()),
            "pattern mismatch for {path}"
        );
    } else {
        assert!(grit_match.is_none());
    }
}

pub struct T0008Repo {
    pub home: PathBuf,
    pub root: PathBuf,
}

impl T0008Repo {
    pub fn setup() -> Self {
        let home = unique_tmp("ignore", "home");
        let root = unique_tmp("ignore", "t0008");
        fs::create_dir_all(&home).expect("home");
        let global = home.join("global-excludes");
        fs::write(&global, "globalone\n!globaltwo\nglobalthree\n").expect("global excludes");

        assert!(
            hermetic_git(&home, &root, &["init"]).status.success(),
            "git init"
        );
        assert!(
            hermetic_git(
                &home,
                &root,
                &["config", "core.excludesfile", global.to_str().unwrap()]
            )
            .status
            .success(),
            "excludesfile"
        );
        assert!(
            hermetic_git(&home, &root, &["config", "core.autocrlf", "false"])
                .status
                .success(),
            "autocrlf"
        );

        fs::create_dir_all(root.join("a/b/ignored-dir")).expect("dirs");
        fs::create_dir_all(root.join("a/submodule")).expect("sub");
        fs::create_dir_all(root.join("b")).expect("b");

        if cfg!(unix) {
            std::os::unix::fs::symlink("../b", root.join("a/symlink")).ok();
        }

        let sub = root.join("a/submodule");
        assert!(
            hermetic_git(&home, &sub, &["init"]).status.success(),
            "sub init"
        );
        fs::write(sub.join("a"), b"a\n").expect("sub file");
        assert!(
            hermetic_git(&home, &sub, &["add", "a"]).status.success(),
            "sub add"
        );
        assert!(
            hermetic_git(&home, &sub, &["commit", "-m", "sub"])
                .status
                .success(),
            "sub commit"
        );

        fs::write(root.join(".gitignore"), "one\nignored-*\ntop-level-dir/\n")
            .expect("root gitignore");

        for dir in ["", "a"] {
            let base = if dir.is_empty() {
                root.clone()
            } else {
                root.join(dir)
            };
            fs::write(base.join("not-ignored"), b"").expect("not-ignored");
            fs::write(base.join("ignored-and-untracked"), b"").expect("iat");
            fs::write(base.join("ignored-but-in-index"), b"").expect("ibi");
        }

        assert!(
            hermetic_git(
                &home,
                &root,
                &[
                    "add",
                    "-f",
                    "ignored-but-in-index",
                    "a/ignored-but-in-index"
                ],
            )
            .status
            .success(),
            "force add"
        );

        fs::write(root.join("a/.gitignore"), "two*\n*three\n").expect("a gitignore");
        fs::write(
            root.join("a/b/.gitignore"),
            "four\nfive\n# comment\nsix\nignored-dir/\n\n!on*\n!two\n",
        )
        .expect("ab gitignore");
        fs::write(root.join("a/b/ignored-dir/.gitignore"), "seven\n").expect("idir gi");
        fs::write(root.join(".git/info/exclude"), "per-repo\n").expect("exclude");
        fs::create_dir_all(root.join("top-level-dir")).expect("tld");
        fs::write(root.join("top-level-dir/file"), b"").expect("tld file");

        assert!(
            hermetic_git(&home, &root, &["add", "a/submodule"])
                .status
                .success(),
            "add sub"
        );

        Self { home, root }
    }

    pub fn open_grit(&self) -> Repository {
        Repository::open(
            &self.root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(&self.root),
        )
        .expect("open grit")
    }
}

pub fn t0008_corpus() -> Vec<(&'static str, bool)> {
    vec![
        ("non-existent", false),
        ("one", true),
        ("not-ignored", false),
        ("ignored-and-untracked", true),
        ("ignored-but-in-index", false),
        ("a/non-existent", false),
        ("a/one", true),
        ("a/not-ignored", false),
        ("a/ignored-and-untracked", true),
        ("a/ignored-but-in-index", false),
        ("a/two", true),
        ("a/athree", true),
        ("a/b/four", true),
        ("a/b/five", true),
        ("a/b/six", true),
        ("a/b/on", false),
        ("a/b/one", true),
        ("a/b/two", false),
        ("a/b/ignored-dir", true),
        ("a/b/ignored-dir/file", true),
        ("globalone", true),
        ("globaltwo", false),
        ("globalthree", true),
        ("per-repo", true),
        ("top-level-dir", true),
        ("top-level-dir/file", true),
    ]
}
