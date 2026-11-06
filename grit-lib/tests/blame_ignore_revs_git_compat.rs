//! Git parity for blame ignore-revision lists (upstream `t8013` intent).

use std::path::Path;
use std::process::Command;

use grit_lib::blame::{blame_file, BlameOptions, IgnoreRevs, IgnoreRevsSources};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Blame Test")
        .env("GIT_AUTHOR_EMAIL", "blame@example.com")
        .env("GIT_COMMITTER_NAME", "Blame Test")
        .env("GIT_COMMITTER_EMAIL", "blame@example.com");
}

fn git_run(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    git_env(&mut cmd);
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    git_env(&mut cmd);
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git_rev_parse(dir: &Path, spec: &str) -> ObjectId {
    let hex = git_output(dir, &["rev-parse", spec]);
    hex.trim().parse().expect("oid")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PorcelainLine {
    final_line: usize,
    commit: ObjectId,
    original_line: usize,
}

fn parse_git_blame_porcelain(output: &str) -> Vec<PorcelainLine> {
    let mut lines = Vec::new();
    let mut group: Option<(ObjectId, usize, usize)> = None;
    for raw in output.lines() {
        if raw.starts_with('\t') {
            let Some((commit, orig, final_line)) = group else {
                continue;
            };
            lines.push(PorcelainLine {
                final_line,
                commit,
                original_line: orig,
            });
            group = Some((commit, orig + 1, final_line + 1));
            continue;
        }
        let mut parts = raw.split_whitespace();
        let Some(sha) = parts.next() else {
            continue;
        };
        if !matches!(sha.len(), 40 | 64) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(commit) = sha.parse::<ObjectId>() else {
            continue;
        };
        let Some(orig_s) = parts.next() else {
            continue;
        };
        let Some(final_s) = parts.next() else {
            continue;
        };
        let Some(_count_s) = parts.next() else {
            continue;
        };
        let Ok(orig) = orig_s.parse::<usize>() else {
            continue;
        };
        let Ok(final_line) = final_s.parse::<usize>() else {
            continue;
        };
        group = Some((commit, orig, final_line));
    }
    lines.sort_by_key(|l| l.final_line);
    lines
}

struct IgnoreParams<'a> {
    ignore_rev: &'a [&'a str],
    ignore_revs_file: &'a [&'a str],
    use_config: bool,
}

fn git_blame(dir: &Path, file: &str, ignore: &IgnoreParams<'_>) -> Vec<PorcelainLine> {
    let mut args: Vec<String> = vec!["blame".into(), "--porcelain".into()];
    for rev in ignore.ignore_rev {
        args.push("--ignore-rev".into());
        args.push((*rev).into());
    }
    for path in ignore.ignore_revs_file {
        args.push("--ignore-revs-file".into());
        args.push((*path).into());
    }
    args.push("--".into());
    args.push(file.into());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git_output(dir, &arg_refs);
    parse_git_blame_porcelain(&out)
}

fn grit_blame(repo: &Repository, file: &str, ignore: &IgnoreParams<'_>) -> Vec<PorcelainLine> {
    let loaded = IgnoreRevs::load(
        repo,
        &IgnoreRevsSources {
            files: ignore
                .ignore_revs_file
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            revs: ignore.ignore_rev.iter().map(|s| (*s).to_owned()).collect(),
            use_config: ignore.use_config,
        },
    )
    .expect("IgnoreRevs::load");
    let result = blame_file(
        repo,
        file,
        &BlameOptions {
            ignore: loaded,
            ..Default::default()
        },
    )
    .expect("blame_file");
    let mut out: Vec<PorcelainLine> = result
        .lines
        .into_iter()
        .map(|line| PorcelainLine {
            final_line: line.final_line,
            commit: line.commit,
            original_line: line.original_line,
        })
        .collect();
    out.sort_by_key(|l| l.final_line);
    out
}

fn assert_blame_matches(repo: &Repository, dir: &Path, file: &str, ignore: &IgnoreParams<'_>) {
    let git_lines = git_blame(dir, file, ignore);
    let grit_lines = grit_blame(repo, file, ignore);
    assert_eq!(git_lines.len(), grit_lines.len(), "line count");
    for (g, r) in git_lines.iter().zip(grit_lines.iter()) {
        assert_eq!(g.final_line, r.final_line, "final line");
        assert_eq!(g.commit, r.commit, "commit at line {}", g.final_line);
        assert_eq!(g.original_line, r.original_line, "original line");
    }
}

fn open_repo(dir: &Path) -> Repository {
    Repository::open(
        &dir.join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir),
    )
    .expect("open repo")
}

fn setup_abcx(dir: &Path) {
    git_run(dir, &["init"]);
    std::fs::write(dir.join("file"), "line1\n").unwrap();
    git_run(dir, &["add", "file"]);
    git_run(dir, &["commit", "-m", "A"]);
    git_run(dir, &["tag", "A"]);
    std::fs::write(dir.join("file"), "line1\nline2\n").unwrap();
    git_run(dir, &["add", "file"]);
    git_run(dir, &["commit", "-m", "B"]);
    git_run(dir, &["tag", "B"]);
    std::fs::write(dir.join("file"), "line-one\nline-two\n").unwrap();
    git_run(dir, &["add", "file"]);
    git_run(dir, &["commit", "-m", "X"]);
    git_run(dir, &["tag", "X"]);
    git_run(dir, &["tag", "-a", "-m", "X (annotated)", "XT"]);
}

#[test]
fn ignore_rev_changing_lines_commit() {
    let dir = tempfile::tempdir().unwrap();
    setup_abcx(dir.path());
    let repo = open_repo(dir.path());
    assert_blame_matches(
        &repo,
        dir.path(),
        "file",
        &IgnoreParams {
            ignore_rev: &["X"],
            ignore_revs_file: &[],
            use_config: false,
        },
    );
}

#[test]
fn ignore_rev_changing_lines_annotated_tag() {
    let dir = tempfile::tempdir().unwrap();
    setup_abcx(dir.path());
    let repo = open_repo(dir.path());
    assert_blame_matches(
        &repo,
        dir.path(),
        "file",
        &IgnoreParams {
            ignore_rev: &["XT"],
            ignore_revs_file: &[],
            use_config: false,
        },
    );
}

#[test]
fn ignore_rev_adding_unblamable_lines() {
    let dir = tempfile::tempdir().unwrap();
    setup_abcx(dir.path());
    std::fs::write(
        dir.path().join("file"),
        "line-one-change\nline-two-changed\ny3\ny4\n",
    )
    .unwrap();
    git_run(dir.path(), &["add", "file"]);
    git_run(dir.path(), &["commit", "-m", "Y"]);
    git_run(dir.path(), &["tag", "Y"]);
    let repo = open_repo(dir.path());
    assert_blame_matches(
        &repo,
        dir.path(),
        "file",
        &IgnoreParams {
            ignore_rev: &["Y"],
            ignore_revs_file: &[],
            use_config: false,
        },
    );
}

#[test]
fn ignore_revs_from_two_files() {
    let dir = tempfile::tempdir().unwrap();
    setup_abcx(dir.path());
    std::fs::write(
        dir.path().join("file"),
        "line-one-change\nline-two-changed\ny3\ny4\n",
    )
    .unwrap();
    git_run(dir.path(), &["add", "file"]);
    git_run(dir.path(), &["commit", "-m", "Y"]);
    git_run(dir.path(), &["tag", "Y"]);
    let x = git_output(dir.path(), &["rev-parse", "X"]);
    let y = git_output(dir.path(), &["rev-parse", "Y"]);
    std::fs::write(dir.path().join("ignore_x"), x.trim()).unwrap();
    std::fs::write(dir.path().join("ignore_y"), y.trim()).unwrap();
    let repo = open_repo(dir.path());
    assert_blame_matches(
        &repo,
        dir.path(),
        "file",
        &IgnoreParams {
            ignore_rev: &[],
            ignore_revs_file: &["ignore_x", "ignore_y"],
            use_config: false,
        },
    );
}

#[test]
fn ignore_revs_config_plus_cli_file() {
    let dir = tempfile::tempdir().unwrap();
    setup_abcx(dir.path());
    std::fs::write(
        dir.path().join("file"),
        "line-one-change\nline-two-changed\ny3\ny4\n",
    )
    .unwrap();
    git_run(dir.path(), &["add", "file"]);
    git_run(dir.path(), &["commit", "-m", "Y"]);
    git_run(dir.path(), &["tag", "Y"]);
    let x = git_output(dir.path(), &["rev-parse", "X"]);
    let y = git_output(dir.path(), &["rev-parse", "Y"]);
    std::fs::write(dir.path().join("ignore_x"), x.trim()).unwrap();
    std::fs::write(dir.path().join("ignore_y"), y.trim()).unwrap();
    git_run(
        dir.path(),
        &["config", "--add", "blame.ignoreRevsFile", "ignore_x"],
    );
    let repo = open_repo(dir.path());
    assert_blame_matches(
        &repo,
        dir.path(),
        "file",
        &IgnoreParams {
            ignore_rev: &[],
            ignore_revs_file: &["ignore_y"],
            use_config: true,
        },
    );
}

#[test]
fn empty_ignore_revs_file_clears_config() {
    let dir = tempfile::tempdir().unwrap();
    setup_abcx(dir.path());
    std::fs::write(
        dir.path().join("file"),
        "line-one-change\nline-two-changed\ny3\ny4\n",
    )
    .unwrap();
    git_run(dir.path(), &["add", "file"]);
    git_run(dir.path(), &["commit", "-m", "Y"]);
    git_run(dir.path(), &["tag", "Y"]);
    let x = git_output(dir.path(), &["rev-parse", "X"]);
    let y = git_output(dir.path(), &["rev-parse", "Y"]);
    std::fs::write(dir.path().join("ignore_x"), x.trim()).unwrap();
    std::fs::write(dir.path().join("ignore_y"), y.trim()).unwrap();
    git_run(
        dir.path(),
        &["config", "--add", "blame.ignoreRevsFile", "ignore_x"],
    );
    let repo = open_repo(dir.path());
    assert_blame_matches(
        &repo,
        dir.path(),
        "file",
        &IgnoreParams {
            ignore_rev: &[],
            ignore_revs_file: &["", "ignore_y"],
            use_config: true,
        },
    );
}

#[test]
fn ignore_merge_commit() {
    let dir = tempfile::tempdir().unwrap();
    git_run(dir.path(), &["init"]);
    std::fs::write(
        dir.path().join("file"),
        "L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nL9\n",
    )
    .unwrap();
    git_run(dir.path(), &["add", "file"]);
    git_run(dir.path(), &["commit", "-m", "A"]);
    std::fs::write(
        dir.path().join("file"),
        "BB\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nL9\n",
    )
    .unwrap();
    git_run(dir.path(), &["add", "file"]);
    git_run(dir.path(), &["commit", "-m", "B"]);
    git_run(dir.path(), &["branch", "topic"]);
    git_run(dir.path(), &["reset", "--hard", "HEAD~1"]);
    std::fs::write(
        dir.path().join("file"),
        "L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nCC\n",
    )
    .unwrap();
    git_run(dir.path(), &["add", "file"]);
    git_run(dir.path(), &["commit", "-m", "C"]);
    git_run(dir.path(), &["merge", "topic", "-m", "M"]);
    git_run(dir.path(), &["tag", "M"]);
    let repo = open_repo(dir.path());
    assert_blame_matches(
        &repo,
        dir.path(),
        "file",
        &IgnoreParams {
            ignore_rev: &["M"],
            ignore_revs_file: &[],
            use_config: false,
        },
    );
}

#[test]
fn ignored_chunk_negative_parent_size() {
    let dir = tempfile::tempdir().unwrap();
    git_run(dir.path(), &["init"]);
    std::fs::write(dir.path().join("file"), "L1\nL2\nL7\nL8\nL9\n").unwrap();
    git_run(dir.path(), &["add", "file"]);
    git_run(dir.path(), &["commit", "-m", "A"]);
    std::fs::write(
        dir.path().join("file"),
        "L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nL9\n",
    )
    .unwrap();
    git_run(dir.path(), &["add", "file"]);
    git_run(dir.path(), &["commit", "-m", "B"]);
    std::fs::write(
        dir.path().join("file"),
        "L1\nL2\nL3\nL4\nxxx\nL6\nL7\nL8\nL9\n",
    )
    .unwrap();
    git_run(dir.path(), &["add", "file"]);
    git_run(dir.path(), &["commit", "-m", "C"]);
    let repo = open_repo(dir.path());
    assert_blame_matches(
        &repo,
        dir.path(),
        "file",
        &IgnoreParams {
            ignore_rev: &["HEAD~1"],
            ignore_revs_file: &[],
            use_config: false,
        },
    );
}

#[test]
fn line_range_through_ignored_commit() {
    let dir = tempfile::tempdir().unwrap();
    setup_abcx(dir.path());
    let repo = open_repo(dir.path());
    let git_lines = git_output(
        dir.path(),
        &[
            "blame",
            "--porcelain",
            "-L",
            "1,2",
            "--ignore-rev",
            "X",
            "--",
            "file",
        ],
    );
    let grit_loaded = IgnoreRevs::load(
        &repo,
        &IgnoreRevsSources {
            revs: vec!["X".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let grit = blame_file(
        &repo,
        "file",
        &BlameOptions {
            line_range: Some(1..=2),
            ignore: grit_loaded,
            ..Default::default()
        },
    )
    .unwrap();
    let parsed = parse_git_blame_porcelain(&git_lines);
    assert_eq!(parsed.len(), grit.lines.len());
    for (g, r) in parsed.iter().zip(grit.lines.iter()) {
        assert_eq!(g.final_line, r.final_line);
        assert_eq!(g.commit, r.commit);
        assert_eq!(g.original_line, r.original_line);
    }
}

#[test]
fn ignore_revs_file_resolves_from_worktree_root_when_cwd_is_subdirectory() {
    let dir = tempfile::tempdir().unwrap();
    setup_abcx(dir.path());
    let x = git_rev_parse(dir.path(), "X");
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/ignore_x"), x.to_hex()).unwrap();
    let sub = dir.path().join("sub");
    let mut env = Environment::capture_process();
    env.cwd = sub.clone();
    env.grit_invocation_cwd = Some(sub.to_string_lossy().into_owned());
    let mut opts = RepositoryOptions::empty().with_work_tree(dir.path());
    opts.environment = env;
    let repo = Repository::open(&dir.path().join(".git"), &opts).unwrap();
    let loaded = IgnoreRevs::load(
        &repo,
        &IgnoreRevsSources {
            files: vec!["sub/ignore_x".into()],
            ..Default::default()
        },
    )
    .expect("load");
    assert!(loaded.as_set().contains(&x));
    let repo_root = open_repo(dir.path());
    assert_blame_matches(
        &repo_root,
        dir.path(),
        "file",
        &IgnoreParams {
            ignore_rev: &["X"],
            ignore_revs_file: &[],
            use_config: false,
        },
    );
}

#[test]
fn ignore_revs_file_skips_absent_full_length_oid() {
    let dir = tempfile::tempdir().unwrap();
    setup_abcx(dir.path());
    std::fs::write(
        dir.path().join("ignore_absent"),
        "0000000000000000000000000000000000000001\n",
    )
    .unwrap();
    let repo = open_repo(dir.path());
    assert_blame_matches(
        &repo,
        dir.path(),
        "file",
        &IgnoreParams {
            ignore_rev: &[],
            ignore_revs_file: &["ignore_absent"],
            use_config: false,
        },
    );
}

#[test]
fn typed_errors_for_bad_rev_and_file() {
    let dir = tempfile::tempdir().unwrap();
    setup_abcx(dir.path());
    let repo = open_repo(dir.path());
    let err = IgnoreRevs::load(
        &repo,
        &IgnoreRevsSources {
            revs: vec!["NOREV".into()],
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(err, Error::BlameIgnoreRevUnknown { .. }));

    let err = IgnoreRevs::load(
        &repo,
        &IgnoreRevsSources {
            files: vec!["NOFILE".into()],
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(err, Error::BlameIgnoreRevsFileOpen { .. }));

    std::fs::write(dir.path().join("ignore_norev"), "NOREV\n").unwrap();
    let err = IgnoreRevs::load(
        &repo,
        &IgnoreRevsSources {
            files: vec!["ignore_norev".into()],
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(err, Error::BlameIgnoreRevsFileInvalidRev { .. }));
}
