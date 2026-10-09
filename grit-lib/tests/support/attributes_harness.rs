//! Helpers for attribute oracle tests against system `git check-attr`.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use grit_lib::attributes::{
    attribute_matching_ignore_case, collect_attrs_for_path,
    load_gitattributes_for_check_attr_cached, load_gitattributes_for_check_attr_source,
    load_gitattributes_stack, normalize_rel_path, path_relative_to_worktree, resolve_tree_oid,
    AttrValue, ParsedGitAttributes,
};
use grit_lib::repo::Repository;

const AUTHOR: &str = "Attributes Harness";
const EMAIL: &str = "attributes-harness@example.com";
const DATE: &str = "1700000000 +0000";

fn null_dev() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Hermetic `git` with fixed identity and no system/global config.
pub fn hermetic_git(worktree: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(worktree)
        .args(args)
        .env("GIT_AUTHOR_NAME", AUTHOR)
        .env("GIT_AUTHOR_EMAIL", EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR)
        .env("GIT_COMMITTER_EMAIL", EMAIL)
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_DATE", DATE)
        .env("GIT_CONFIG_GLOBAL", null_dev())
        .env("GIT_CONFIG_SYSTEM", null_dev())
        .env_remove("GIT_ATTR_SOURCE")
        .output()
        .expect("spawn git")
}

pub fn hermetic_git_ok(worktree: &Path, args: &[&str]) -> String {
    let out = hermetic_git(worktree, args);
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn grit_repo(worktree: &Path) -> Repository {
    Repository::open(&worktree.join(".git"), Some(worktree)).expect("open grit repo")
}

pub fn ignore_case(repo: &Repository) -> bool {
    attribute_matching_ignore_case(repo)
}

pub fn attr_display(v: Option<&AttrValue>) -> &'static str {
    match v {
        None => "unspecified",
        Some(AttrValue::Set) => "set",
        Some(AttrValue::Unset) => "unset",
        Some(AttrValue::Clear) => "unspecified",
        Some(AttrValue::Value(s)) => {
            if s.is_empty() {
                "unspecified"
            } else {
                // Values are compared as strings in callers.
                "value"
            }
        }
    }
}

pub fn grit_attr_value(
    parsed: &ParsedGitAttributes,
    repo: &Repository,
    rel: &str,
    name: &str,
) -> String {
    let icase = ignore_case(repo);
    let map = collect_attrs_for_path(&parsed.rules, &parsed.macros, rel, icase);
    attr_value_to_git_string(map.get(name))
}

/// Map one [`AttrValue`] to `git check-attr` text (`unspecified` attrs are omitted from `-a` output).
pub fn attr_value_to_git_string(v: Option<&AttrValue>) -> String {
    match v {
        None => "unspecified".to_string(),
        Some(AttrValue::Set) => "set".to_string(),
        Some(AttrValue::Unset) => "unset".to_string(),
        Some(AttrValue::Clear) => "unspecified".to_string(),
        Some(AttrValue::Value(s)) => s.clone(),
    }
}

/// All non-unspecified attributes Grit would report for `rel_path` (keys sorted for stable diffs).
pub fn grit_all_attrs_for_path(
    parsed: &ParsedGitAttributes,
    repo: &Repository,
    rel: &str,
) -> BTreeMap<String, String> {
    let icase = ignore_case(repo);
    let map = collect_attrs_for_path(&parsed.rules, &parsed.macros, rel, icase);
    map.iter()
        .map(|(k, v)| (k.clone(), attr_value_to_git_string(Some(v))))
        .filter(|(_, v)| v != "unspecified")
        .collect()
}

/// Parse `git check-attr -z` records (`path\\0attr\\0value\\0`).
pub fn parse_check_attr_z(stdout: &[u8], attr: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut parts = stdout.split(|&b| b == 0).filter(|p| !p.is_empty());
    while let Some(path) = parts.next() {
        let Some(name) = parts.next() else {
            break;
        };
        let Some(val) = parts.next() else {
            break;
        };
        if name == attr.as_bytes() {
            let p = String::from_utf8_lossy(path).into_owned();
            let v = String::from_utf8_lossy(val).into_owned();
            out.insert(p, v);
        }
    }
    out
}

/// Parse `git check-attr -a -z --stdin` output: for each input path, a map of attribute → value.
///
/// Git emits `path\\0attr\\0value\\0` per assigned attribute; paths with no attributes produce
/// no records (the path is omitted entirely).
pub fn parse_check_attr_all_z(
    stdout: &[u8],
    paths: &[&str],
) -> BTreeMap<String, BTreeMap<String, String>> {
    let tokens: Vec<&[u8]> = stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    let mut parsed: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut i = 0usize;
    while i + 2 < tokens.len() {
        let path = String::from_utf8_lossy(tokens[i]).into_owned();
        let attr = String::from_utf8_lossy(tokens[i + 1]).into_owned();
        let val = String::from_utf8_lossy(tokens[i + 2]).into_owned();
        i += 3;
        parsed.entry(path).or_default().insert(attr, val);
    }
    paths
        .iter()
        .map(|p| ((*p).to_string(), parsed.remove(*p).unwrap_or_default()))
        .collect()
}

pub fn git_check_attr_all_z(
    worktree: &Path,
    paths: &[&str],
    extra_args: &[&str],
) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut cmd = Command::new("git");
    cmd.current_dir(worktree)
        .arg("check-attr")
        .args(extra_args)
        .arg("-a")
        .arg("-z")
        .arg("--stdin")
        .env("GIT_AUTHOR_NAME", AUTHOR)
        .env("GIT_AUTHOR_EMAIL", EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR)
        .env("GIT_COMMITTER_EMAIL", EMAIL)
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_DATE", DATE)
        .env("GIT_CONFIG_GLOBAL", null_dev())
        .env("GIT_CONFIG_SYSTEM", null_dev())
        .env_remove("GIT_ATTR_SOURCE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn check-attr -a -z");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("stdin");
        for p in paths {
            stdin.write_all(p.as_bytes()).expect("write path");
            stdin.write_all(&[0]).expect("write nul");
        }
    }
    let out = child.wait_with_output().expect("wait check-attr");
    assert!(
        out.status.success(),
        "check-attr -a -z failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_check_attr_all_z(&out.stdout, paths)
}

pub fn git_check_attr_z(
    worktree: &Path,
    attr: &str,
    paths: &[&str],
    extra_args: &[&str],
) -> BTreeMap<String, String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(worktree)
        .arg("check-attr")
        .args(extra_args)
        .arg("-z")
        .arg("--stdin")
        .arg(attr)
        .env("GIT_AUTHOR_NAME", AUTHOR)
        .env("GIT_AUTHOR_EMAIL", EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR)
        .env("GIT_COMMITTER_EMAIL", EMAIL)
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_DATE", DATE)
        .env("GIT_CONFIG_GLOBAL", null_dev())
        .env("GIT_CONFIG_SYSTEM", null_dev())
        .env_remove("GIT_ATTR_SOURCE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn check-attr");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("stdin");
        for p in paths {
            stdin.write_all(p.as_bytes()).expect("write path");
            stdin.write_all(&[0]).expect("write nul");
        }
    }
    let out = child.wait_with_output().expect("wait check-attr");
    assert!(
        out.status.success(),
        "check-attr failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_check_attr_z(&out.stdout, attr)
}

pub fn git_check_attr_paths(
    worktree: &Path,
    attr: &str,
    paths: &[&str],
    extra_args: &[&str],
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for p in paths {
        let mut args: Vec<&str> = Vec::with_capacity(4 + extra_args.len());
        args.push("check-attr");
        args.extend(extra_args);
        args.push(attr);
        args.extend(["--", p]);
        let line = hermetic_git_ok(worktree, &args);
        let expect_prefix = format!("{p}: {attr}: ");
        for l in line.lines() {
            if let Some(rest) = l.strip_prefix(&expect_prefix) {
                map.insert(p.to_string(), rest.to_string());
            }
        }
    }
    map
}

pub fn assert_attrs_match(
    worktree: &Path,
    repo: &Repository,
    parsed: &ParsedGitAttributes,
    attr: &str,
    paths: &[&str],
    git_extra: &[&str],
) {
    let git_map = git_check_attr_paths(worktree, attr, paths, git_extra);
    for p in paths {
        let rel = normalize_rel_path(p);
        let grit_v = grit_attr_value(parsed, repo, &rel, attr);
        let git_v = git_map.get(*p).cloned().unwrap_or_else(|| "missing".into());
        assert_eq!(
            grit_v, git_v,
            "attr {attr} path {p} (git extra {git_extra:?})"
        );
    }
}

/// Compare every attribute `git check-attr -a -z --stdin` returns for each path with Grit.
pub fn assert_all_attrs_match(
    worktree: &Path,
    repo: &Repository,
    parsed: &ParsedGitAttributes,
    paths: &[&str],
    git_extra: &[&str],
) {
    let git_by_path = git_check_attr_all_z(worktree, paths, git_extra);
    for p in paths {
        let rel = normalize_rel_path(p);
        let grit_map = grit_all_attrs_for_path(parsed, repo, &rel);
        let git_map = git_by_path.get(*p).cloned().unwrap_or_default();
        assert_eq!(
            grit_map, git_map,
            "all attrs path {p} (git extra {git_extra:?})"
        );
    }
}

/// Build the upstream `t0003-attributes` `setup` work tree (without global file).
pub fn setup_t0003_worktree(root: &Path) {
    std::fs::create_dir_all(root.join("a/b/d")).expect("mkdir");
    std::fs::create_dir_all(root.join("a/c")).expect("mkdir");
    std::fs::create_dir_all(root.join("b")).expect("mkdir");
    std::fs::write(
        root.join(".gitattributes"),
        "[attr]notest !test\n\
         \" d \"\ttest=d\n\
          e\ttest=e\n\
          e\"\ttest=e\n\
         f\ttest=f\n\
         a/i test=a/i\n\
         onoff test -test\n\
         offon -test test\n\
         no notest\n\
         A/e/F test=A/e/F\n",
    )
    .expect("root ga");
    std::fs::write(
        root.join("a/.gitattributes"),
        "g test=a/g\nb/g test=a/b/g\n",
    )
    .expect("a ga");
    std::fs::write(
        root.join("a/b/.gitattributes"),
        "h test=a/b/h\n\
         d/* test=a/b/d/*\n\
         d/yes notest\n",
    )
    .expect("ab ga");
}

pub fn init_git_repo(root: &Path) {
    hermetic_git_ok(root, &["init", "-q"]);
}

pub fn setup_t0003_tags(root: &Path) {
    std::fs::create_dir_all(root.join("foo/bar")).expect("mkdir foo/bar");
    std::fs::write(
        root.join("foo/bar/.gitattributes"),
        "f test=f\na/i test=n\n",
    )
    .expect("tag1 ga");
    hermetic_git_ok(root, &["add", "foo/bar/.gitattributes"]);
    hermetic_git_ok(root, &["commit", "-m", "add tag-1 gitattributes"]);
    hermetic_git_ok(root, &["tag", "tag-1"]);
    std::fs::write(
        root.join("foo/bar/.gitattributes"),
        "g test=g\na/i test=m\n",
    )
    .expect("tag2 ga");
    hermetic_git_ok(root, &["add", "foo/bar/.gitattributes"]);
    hermetic_git_ok(root, &["commit", "-m", "add tag-2 gitattributes"]);
    hermetic_git_ok(root, &["tag", "tag-2"]);
    let _ = std::fs::remove_file(root.join("foo/bar/.gitattributes"));
}

pub fn t0003_corpus_paths() -> Vec<&'static str> {
    vec![
        "f",
        "a/f",
        "a/c/f",
        "a/g",
        "a/b/g",
        "b/g",
        "a/b/h",
        "a/b/d/g",
        "onoff",
        "offon",
        "no",
        "a/b/d/no",
        "a/b/d/yes",
    ]
}

pub fn t0003_source_paths() -> Vec<&'static str> {
    vec!["foo/bar/f", "foo/bar/a/i", "foo/bar/g"]
}

pub fn load_worktree_parsed(repo: &Repository, wt: &Path) -> ParsedGitAttributes {
    load_gitattributes_stack(repo, wt).expect("worktree stack")
}

pub fn load_cached_parsed(repo: &Repository) -> ParsedGitAttributes {
    load_gitattributes_for_check_attr_cached(repo).expect("cached stack")
}

pub fn load_source_parsed(repo: &Repository, treeish: &str) -> ParsedGitAttributes {
    let oid = resolve_tree_oid(repo, treeish).expect("resolve tree");
    load_gitattributes_for_check_attr_source(repo, &oid).expect("source stack")
}

pub fn rel_path_for_check(repo: &Repository, path: &str) -> String {
    path_relative_to_worktree(repo, path).unwrap_or_else(|_| normalize_rel_path(path))
}
