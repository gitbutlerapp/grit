//! `Repository::paths_match_pathspecs` evaluates `:(attr:...)` magic against the repository's
//! `.gitattributes`, cross-checked against `git ls-files <specs>` (t6135 subset).

use grit_lib::worktree::WorktreeError;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{AttrPathspecError, Error};
use grit_lib::repo::Repository;
use grit_lib::test_support::pathspec::pathspec_has_attr_magic;
use grit_lib::worktree::pathspec::{validate_attr_pathspecs, Pathspec, PathspecGlobalFlags};
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_ls_files(root: &Path, specs: &[&str]) -> Vec<String> {
    let mut args = vec!["ls-files", "--"];
    args.extend_from_slice(specs);
    git(root, &args).lines().map(str::to_owned).collect()
}

fn init_attr_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    let files: &[(&str, &str)] = &[
        (
            ".gitattributes",
            "src/**/*.rs diff=rust linguist-generated\n*.bin -diff\nnotes/* !diff\n",
        ),
        ("src/sub/a.rs", "a\n"),
        ("src/b.rs", "b\n"),
        ("lib/c.rs", "c\n"),
        ("data.bin", "bin\n"),
        ("notes/n.txt", "n\n"),
        ("README.md", "r\n"),
        ("sub/.gitattributes", "*.txt diff=text\n"),
        ("sub/t.txt", "t\n"),
    ];
    for (path, content) in files {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

fn grit_matches(repo: &Repository, all: &[String], specs: &[&str]) -> Vec<String> {
    let specs: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
    let pathspec = Pathspec::scoped(specs, PathspecGlobalFlags::default());
    all.iter()
        .filter(|p| repo.paths_match_pathspecs(p, &pathspec).expect("match"))
        .cloned()
        .collect()
}

#[test]
fn attr_pathspecs_match_git_ls_files() {
    let dir = init_attr_repo();
    let root = dir.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let all = git_ls_files(root, &[]);

    let cases: &[&[&str]] = &[
        &[":(attr:diff=rust)"],
        &[":(attr:-diff)"],
        &[":(attr:!diff)"],
        &[":(attr:diff)"],
        &[":(attr:linguist-generated)"],
        &[":(attr:diff=rust linguist-generated)"],
        &[":(attr:diff=text)"],
        &[":(attr:diff=rust)src/sub"],
        &[":(attr:diff=rust)", ":(exclude)src/sub"],
        &["*", ":(exclude,attr:diff=rust)"],
        &[":(exclude,attr:-diff)"],
        &[":(exclude,attr:!diff)"],
        &[":(attr:-diff)", "README.md"],
        &[":(top,attr:diff=rust)"],
        &[":(icase,attr:diff=rust)SRC"],
    ];
    for specs in cases {
        let expected = git_ls_files(root, specs);
        let got = grit_matches(&repo, &all, specs);
        assert_eq!(got, expected, "pathspecs {specs:?}");
    }
    assert_eq!(
        git_ls_files(root, &[":(attr:diff=rust)"]),
        vec!["src/b.rs".to_owned(), "src/sub/a.rs".to_owned()],
        "fixture sanity: git must select the rust files"
    );
}

#[test]
fn non_attr_pathspecs_still_match_git_ls_files() {
    let dir = init_attr_repo();
    let root = dir.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let all = git_ls_files(root, &[]);
    let cases: &[&[&str]] = &[
        &["src"],
        &["*.rs"],
        &[":(glob)src/**/*.rs"],
        &[":(exclude)src"],
        &[":(icase)readme.MD"],
        &[":(literal)src/b.rs"],
    ];
    for specs in cases {
        assert_eq!(
            grit_matches(&repo, &all, specs),
            git_ls_files(root, specs),
            "pathspecs {specs:?}"
        );
    }
}

#[test]
fn attr_pathspecs_see_gitattributes_edits_on_the_same_handle() {
    let dir = init_attr_repo();
    let root = dir.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let spec = vec![":(attr:diff=rust)".to_owned()];
    let pathspec = Pathspec::scoped(spec, PathspecGlobalFlags::default());
    assert!(!repo.paths_match_pathspecs("lib/c.rs", &pathspec).unwrap());
    std::fs::write(root.join(".gitattributes"), "*.rs diff=rust\n").unwrap();
    assert!(repo.paths_match_pathspecs("lib/c.rs", &pathspec).unwrap());
}

#[test]
fn attr_pathspecs_in_bare_repository_use_info_attributes() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "--bare", "-b", "main", "."]);
    std::fs::create_dir_all(root.join("info")).unwrap();
    std::fs::write(root.join("info/attributes"), "*.rs diff=rust\n").unwrap();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let spec = vec![":(attr:diff=rust)".to_owned()];
    let pathspec = Pathspec::scoped(spec, PathspecGlobalFlags::default());
    assert!(repo.paths_match_pathspecs("src/a.rs", &pathspec).unwrap());
    assert!(!repo.paths_match_pathspecs("README.md", &pathspec).unwrap());
}

#[test]
fn pathspec_matcher_matches_paths_and_renames() {
    let dir = init_attr_repo();
    let root = dir.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let all = git_ls_files(root, &[]);

    let specs = vec![":(attr:diff=rust)".to_owned()];
    let pathspec = Pathspec::scoped(specs, PathspecGlobalFlags::default());
    assert!(!pathspec.is_entire_tree());
    let got: Vec<_> = all
        .iter()
        .filter(|p| repo.paths_match_pathspecs(p, &pathspec).expect("match"))
        .cloned()
        .collect();
    assert_eq!(got, git_ls_files(root, &[":(attr:diff=rust)"]));
    assert!(pathspec
        .matches_change(&repo, "lib/c.rs", Some("src/b.rs"))
        .expect("change"));
    assert!(pathspec
        .matches_change(&repo, "src/b.rs", None)
        .expect("change"));
    assert!(!pathspec
        .matches_change(&repo, "lib/c.rs", Some("lib/c.rs"))
        .expect("change"));
    assert!(!pathspec
        .matches_change(&repo, "README.md", None)
        .expect("change"));

    let entire = Pathspec::entire_tree(PathspecGlobalFlags::default());
    assert!(entire.is_entire_tree());
    assert!(entire
        .matches_change(&repo, "anything", None)
        .expect("change"));

    let plain = vec!["src".to_owned(), ":(exclude)src/sub".to_owned()];
    let plain_ps = Pathspec::scoped(plain, PathspecGlobalFlags::default());
    assert!(repo.paths_match_pathspecs("src/b.rs", &plain_ps).unwrap());
    assert!(!repo
        .paths_match_pathspecs("src/sub/a.rs", &plain_ps)
        .unwrap());
    assert!(plain_ps
        .matches_change(&repo, "lib/x.rs", Some("src/b.rs"))
        .expect("change"));
}

fn init_repo_with(files: &[(&str, &str)], config: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    for (key, value) in config {
        git(root, &["config", key, value]);
    }
    for (path, content) in files {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
    dir
}

fn assert_cases_match_git(root: &Path, cases: &[&[&str]]) {
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let all = git_ls_files(root, &[]);
    for specs in cases {
        let expected = git_ls_files(root, specs);
        assert!(
            !expected.is_empty(),
            "fixture sanity: git selects {specs:?}"
        );
        assert_eq!(
            grit_matches(&repo, &all, specs),
            expected,
            "pathspecs {specs:?}"
        );
    }
}

#[test]
fn attr_pathspecs_expand_attribute_macros_like_git() {
    let dir = init_repo_with(
        &[
            (
                ".gitattributes",
                "[attr]rusty diff=rust linguist-generated\n*.rs rusty\n*.dat binary\n*.cfg diff=\n",
            ),
            ("macro.rs", "m\n"),
            ("empty.cfg", "e\n"),
            ("plain.txt", "p\n"),
            ("blob.dat", "d\n"),
        ],
        &[],
    );
    assert_cases_match_git(
        dir.path(),
        &[
            &[":(attr:diff=rust)"],
            &[":(attr:rusty)"],
            &[":(attr:linguist-generated)"],
            &[":(attr:binary)"],
            &[":(attr:diff=)"],
            &[":(attr:-diff)"],
            &[":(attr:-text)"],
            &[":(attr:!diff)"],
            &[":(exclude,attr:diff=rust)"],
        ],
    );
}

#[test]
fn attr_pathspecs_follow_core_ignorecase_like_git() {
    let dir = init_repo_with(
        &[
            (".gitattributes", "*.UPPER diff=upper\n"),
            ("case.upper", "c\n"),
            ("other.txt", "o\n"),
        ],
        &[("core.ignorecase", "true")],
    );
    assert_cases_match_git(
        dir.path(),
        &[
            &[":(attr:diff=upper)"],
            &[":(attr:!diff)"],
            &[":(exclude,attr:diff=upper)"],
        ],
    );

    let sensitive = init_repo_with(
        &[
            (".gitattributes", "*.UPPER diff=upper\n"),
            ("case.upper", "c\n"),
        ],
        &[("core.ignorecase", "false")],
    );
    let repo =
        Repository::discover(Some(sensitive.path()), &RepositoryOptions::empty()).expect("open");
    let spec = vec![":(attr:diff=upper)".to_owned()];
    let pathspec = Pathspec::scoped(spec, PathspecGlobalFlags::default());
    assert!(!repo.paths_match_pathspecs("case.upper", &pathspec).unwrap());
    assert!(git_ls_files(sensitive.path(), &[":(attr:diff=upper)"]).is_empty());
}

#[test]
fn malformed_attr_pathspecs_are_rejected_like_git() {
    let dir = init_attr_repo();
    let root = dir.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let cases: &[(&str, AttrPathspecError)] = &[
        (":(attr:)", AttrPathspecError::Empty),
        (
            ":(attr:diff,attr:text)",
            AttrPathspecError::MultipleAttrClauses,
        ),
        (
            ":(exclude,attr:diff,attr:-text)",
            AttrPathspecError::MultipleAttrClauses,
        ),
        (
            ":(attr:bad/name)",
            AttrPathspecError::InvalidName {
                name: "bad/name".to_owned(),
            },
        ),
        (
            ":(attr:-a=b)",
            AttrPathspecError::InvalidName {
                name: "a=b".to_owned(),
            },
        ),
    ];
    for (spec, reason) in cases {
        let out = Command::new("git")
            .current_dir(root)
            .args(["ls-files", "--", spec])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("run git");
        assert_eq!(out.status.code(), Some(128), "git must reject {spec}");

        let specs = vec![(*spec).to_owned()];
        match validate_attr_pathspecs(&specs) {
            Err(Error::Worktree(WorktreeError::InvalidAttrPathspec {
                spec: got,
                reason: why,
            })) => {
                assert_eq!(got, *spec);
                assert_eq!(&why, reason, "{spec}");
            }
            other => panic!("{spec}: expected InvalidAttrPathspec, got {other:?}"),
        }
        let pathspec = Pathspec::scoped(specs.clone(), PathspecGlobalFlags::default());
        assert!(repo.paths_match_pathspecs("src/b.rs", &pathspec).is_err());
        assert!(validate_attr_pathspecs(&specs).is_err());
        assert!(pathspec_has_attr_magic(spec), "{spec}");
    }
    assert!(validate_attr_pathspecs(&[":(attr:diff=rust)".to_owned(), "src".to_owned()]).is_ok());

    // Whitespace alone is an attr clause with no requirements: Git accepts it as a plain pattern.
    let all = git_ls_files(root, &[]);
    for spec in [":(attr:  )src", ":(attr: )"] {
        assert_eq!(
            grit_matches(&repo, &all, &[spec]),
            git_ls_files(root, &[spec]),
            "{spec}"
        );
    }
}

#[test]
fn has_attr_magic_detects_long_magic_only() {
    assert!(pathspec_has_attr_magic(":(attr:diff=rust)"));
    assert!(pathspec_has_attr_magic(":(exclude,attr:-diff)src"));
    assert!(pathspec_has_attr_magic(":(top,attr:!diff)"));
    assert!(!pathspec_has_attr_magic(":(exclude)src"));
    assert!(!pathspec_has_attr_magic(":!attr"));
    assert!(!pathspec_has_attr_magic("attr:diff"));
    assert!(!pathspec_has_attr_magic("src/*.rs"));
}
