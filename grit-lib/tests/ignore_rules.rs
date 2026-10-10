//! Ignore rules and wildmatch integration (t0008, t3001, t3003): cross-check
//! [`IgnoreMatcher::check_path`] against system `git check-ignore`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use grit_lib::ignore::{
    parse_sparse_patterns_from_blob, path_in_sparse_checkout, path_matches_sparse_pattern_list,
    submodule_containing_path, IgnoreMatch, IgnoreMatcher,
};
use grit_lib::index::Index;
use grit_lib::repo::Repository;
use grit_test_support::{git_cmd, unique_tmp};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const AUTHOR: &str = "Ignore Test Author";
const EMAIL: &str = "ignore-test@example.com";
const DATE: &str = "1700000000 +0000";

struct GitCheckLine {
    ignored: bool,
    source: Option<String>,
    line: Option<usize>,
    pattern: Option<String>,
}

fn hermetic_git(home: &Path, repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("HOME", home)
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

fn git_check_ignore_verbose(
    home: &Path,
    repo: &Path,
    paths: &[&str],
    no_index: bool,
) -> HashMap<String, GitCheckLine> {
    let mut map = HashMap::new();
    for path in paths {
        let mut full_args = vec!["check-ignore", "-v", "-n"];
        if no_index {
            full_args.push("--no-index");
        }
        full_args.push("--");
        full_args.push(path);
        let out = git_cmd(&full_args)
            .env("HOME", home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .in_dir(repo)
            .exec();
        let code = out.status.unwrap_or(-1);
        assert!(
            code == 0 || code == 1,
            "git check-ignore {path}: {}",
            out.dump("git")
        );
        map.extend(parse_git_check_ignore_output(&out.stdout));
    }
    map
}

fn parse_git_check_ignore_output(stdout: &str) -> HashMap<String, GitCheckLine> {
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

fn grit_check(
    repo: &Repository,
    matcher: &mut IgnoreMatcher,
    index: Option<&Index>,
    path: &str,
    is_dir: bool,
) -> (bool, Option<IgnoreMatch>) {
    matcher
        .check_path(repo, index, path, is_dir)
        .expect("check_path")
}

fn assert_match_eq(
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
        assert!(
            m.source_display.ends_with(gsrc.trim_start_matches('/'))
                || gsrc.ends_with(&m.source_display)
                || m.source_display == *gsrc,
            "source mismatch for {path}: grit={} git={gsrc}",
            m.source_display
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
        // Git -v still attributes the negating rule when `-n` is set.
        let m = grit_match
            .as_ref()
            .expect("grit match metadata for negated non-match");
        assert!(m.negative, "expected negated match metadata for {path}");
        let gsrc = git.source.as_ref().expect("git source for negated match");
        assert!(
            m.source_display.ends_with(gsrc.trim_start_matches('/'))
                || gsrc.ends_with(&m.source_display)
                || m.source_display == *gsrc,
            "source mismatch for {path}: grit={} git={gsrc}",
            m.source_display
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

struct T0008Repo {
    home: PathBuf,
    root: PathBuf,
}

impl T0008Repo {
    fn setup() -> Self {
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

    fn open_grit(&self) -> Repository {
        Repository::open(&self.root.join(".git"), Some(&self.root)).expect("open grit")
    }
}

fn t0008_corpus() -> Vec<(&'static str, bool)> {
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

#[test]
fn parse_git_check_ignore_verbose_format() {
    let stdout = "::\tnon-existent\n.gitignore:1:one\tone\n";
    let map = parse_git_check_ignore_output(stdout);
    assert!(map.get("non-existent").is_some_and(|l| !l.ignored));
    assert!(map.get("one").is_some_and(|l| l.ignored));
}

#[test]
fn t0008_check_path_matches_git_check_ignore_verbose() {
    let fx = T0008Repo::setup();
    let repo = fx.open_grit();
    let mut matcher = IgnoreMatcher::from_repository(&repo).expect("matcher");
    let index = repo.load_index().ok();

    let paths: Vec<String> = t0008_corpus()
        .iter()
        .map(|(p, _)| (*p).to_owned())
        .collect();
    let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();

    let git_map = git_check_ignore_verbose(&fx.home, &fx.root, &path_refs, false);
    for (path, _expect) in t0008_corpus() {
        let git = git_map
            .get(path)
            .unwrap_or_else(|| panic!("git missing {path}"));
        let is_dir = path.ends_with('/') || fx.root.join(path).is_dir();
        let (ignored, m) = grit_check(&repo, &mut matcher, index.as_ref(), path, is_dir);
        assert_match_eq(path, git, ignored, &m);
    }

    let git_no_index = git_check_ignore_verbose(&fx.home, &fx.root, &path_refs, true);
    for (path, _) in t0008_corpus() {
        let git = git_no_index
            .get(path)
            .unwrap_or_else(|| panic!("git no-index missing {path}"));
        let is_dir = path.ends_with('/') || fx.root.join(path).is_dir();
        let (ignored, m) = grit_check(&repo, &mut matcher, None, path, is_dir);
        assert_match_eq(path, git, ignored, &m);
    }
}

#[test]
fn t0008_cli_exclude_precedence_over_gitignore() {
    let fx = T0008Repo::setup();
    let repo = fx.open_grit();
    let mut matcher = IgnoreMatcher::from_repository(&repo).expect("matcher");
    matcher.add_cli_excludes(&["!one".to_owned()]);

    // Git 2.43 lacks `check-ignore -x`; compare grit negation via `--exclude` file parity:
    // CLI `-x !one` is equivalent to adding a high-precedence rule.
    let paths = ["one", "a/one"];
    for path in paths {
        let (ignored, _) = grit_check(&repo, &mut matcher, None, path, false);
        assert!(!ignored, "{path} should be re-included by CLI !one");
    }
}

#[test]
fn t0008_symlinked_gitignore_emits_warning_and_is_skipped() {
    if !cfg!(unix) {
        return;
    }
    let root = unique_tmp("ignore", "symlink-gi");
    git_cmd(&["init"]).in_dir(&root).suc();
    fs::write(root.join("target"), b"x").expect("target");
    std::os::unix::fs::symlink("target", root.join(".gitignore")).expect("symlink");
    fs::write(root.join("ignored"), b"").expect("ignored");
    fs::write(root.join(".gitignore-real"), "ignored\n").expect("real");

    let repo = Repository::open(&root.join(".git"), Some(&root)).expect("open");
    let mut matcher = IgnoreMatcher::from_repository(&repo).expect("matcher");
    let (ignored, _) = matcher
        .check_path(&repo, None, "ignored", false)
        .expect("check");
    assert!(!ignored, "symlink .gitignore must not exclude (t0008)");
    let warnings = matcher.take_warnings();
    assert!(
        warnings.iter().any(|w| w.contains("symbolic links")),
        "expected symlink warning, got {warnings:?}"
    );
}

#[test]
fn sparse_helpers_match_git_sparse_checkout_non_cone() {
    let root = unique_tmp("ignore", "sparse");
    let home = unique_tmp("ignore", "home3");
    assert!(
        hermetic_git(&home, &root, &["init"]).status.success(),
        "init"
    );
    assert!(
        hermetic_git(&home, &root, &["sparse-checkout", "init", "--no-cone"])
            .status
            .success(),
        "sparse init"
    );
    assert!(
        hermetic_git(
            &home,
            &root,
            &["sparse-checkout", "set", "/*", "!/*/", "sub/"],
        )
        .status
        .success(),
        "sparse set"
    );

    let patterns_blob = fs::read_to_string(root.join(".git/info/sparse-checkout")).expect("read");
    let lines = parse_sparse_patterns_from_blob(&patterns_blob);

    let cases = [("a", true), ("deep/here", false)];
    for (path, want) in cases {
        let grit = path_in_sparse_checkout(path, &lines, None);
        assert_eq!(grit, want, "path_in_sparse_checkout {path}");
        let listed = path_matches_sparse_pattern_list(path, &lines);
        if path == "deep/here" {
            assert_eq!(listed, None, "undecided flat list for deep path");
        } else {
            assert_eq!(
                listed,
                Some(want),
                "path_matches_sparse_pattern_list {path}"
            );
        }
    }
    // `sub/` include line: cone parent walk vs flat list helper can differ; check inclusion only.
    assert!(path_in_sparse_checkout("sub/file", &lines, None));
}

#[test]
fn normalize_repo_relative_and_submodule_containing_path() {
    let fx = T0008Repo::setup();
    let repo = fx.open_grit();
    let index = repo.load_index().expect("index");
    let sub = submodule_containing_path("a/submodule/x", &index);
    assert_eq!(sub.as_deref(), Some("a/submodule"));

    let cwd = fx.root.join("a");
    let rel = grit_lib::ignore::normalize_repo_relative(&repo, &cwd, "b/four").expect("normalize");
    assert_eq!(rel, "a/b/four");
}

#[test]
fn t3001_exclude_from_and_per_directory_name() {
    let root = unique_tmp("ignore", "t3001");
    let home = unique_tmp("ignore", "home4");
    assert!(
        hermetic_git(&home, &root, &["init"]).status.success(),
        "init"
    );
    fs::write(root.join("custom-ignore"), "from-root\ncustom\n").expect("custom");
    fs::write(root.join("exclude-from"), "from-file\n").expect("ex");
    fs::create_dir_all(root.join("sub")).expect("sub");
    fs::write(root.join("sub/custom-ignore"), "sub-custom\n").expect("sub ci");
    fs::write(root.join("from-root"), b"").expect("touch");
    fs::write(root.join("from-file"), b"").expect("touch");
    fs::write(root.join("custom"), b"").expect("touch");
    fs::write(root.join("sub/custom"), b"").expect("touch");

    let repo = Repository::open(&root.join(".git"), Some(&root)).expect("open");
    let mut matcher = IgnoreMatcher::from_repository(&repo).expect("matcher");
    matcher.set_per_directory_name("custom-ignore");
    matcher
        .add_exclude_from_files(&[root.join("exclude-from")], &root)
        .expect("exclude-from");

    let cases: [(&str, bool); 4] = [
        ("from-root", true),
        ("from-file", true),
        ("custom", true),
        ("sub/custom", true),
    ];
    for (path, want) in cases {
        let (ignored, _) = grit_check(&repo, &mut matcher, None, path, false);
        assert_eq!(ignored, want, "{path}");
    }
    let _ = home;
}

#[test]
fn t3003_crlf_gitignore_lines() {
    let root = unique_tmp("ignore", "crlf");
    git_cmd(&["init"]).in_dir(&root).suc();
    fs::write(root.join(".gitignore"), b"crlf\r\n").expect("crlf gi");
    fs::write(root.join("crlf"), b"").expect("file");

    let repo = Repository::open(&root.join(".git"), Some(&root)).expect("open");
    let mut matcher = IgnoreMatcher::from_repository(&repo).expect("matcher");
    let home = unique_tmp("ignore", "crlf-home");
    let out = git_cmd(&["check-ignore", "-v", "-n", "crlf"])
        .env("HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .in_dir(&root)
        .suc();
    let git_map = parse_git_check_ignore_output(&out.stdout);
    let git = git_map.get("crlf").expect("git");
    let (ignored, m) = grit_check(&repo, &mut matcher, None, "crlf", false);
    assert_match_eq("crlf", git, ignored, &m);
}
