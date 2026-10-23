//! `-L` line range resolution matches `git blame --porcelain -L …`.

use std::path::Path;
use std::process::Command;

use grit_lib::blame::{blame_file, resolve_blame_line_range, BlameOptions};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Blame Range Test")
        .env("GIT_AUTHOR_EMAIL", "blame-range@example.com")
        .env("GIT_COMMITTER_NAME", "Blame Range Test")
        .env("GIT_COMMITTER_EMAIL", "blame-range@example.com");
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

fn git_blame_range(dir: &Path, file: &str, range_spec: &str) -> Vec<PorcelainLine> {
    let out = git_output(dir, &["blame", "--porcelain", "-L", range_spec, "--", file]);
    parse_git_blame_porcelain(&out)
}

fn grit_blame_range(
    repo: &Repository,
    file: &str,
    start: ObjectId,
    range_spec: &str,
) -> Vec<PorcelainLine> {
    let range = resolve_blame_line_range(repo, start, file, range_spec).expect("resolve range");
    let result = blame_file(
        repo,
        file,
        &BlameOptions {
            start: Some(start),
            line_range: Some(range),
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

fn assert_range_matches_git(
    repo: &Repository,
    dir: &Path,
    file: &str,
    start: ObjectId,
    range_spec: &str,
) {
    let git_lines = git_blame_range(dir, file, range_spec);
    let grit_lines = grit_blame_range(repo, file, start, range_spec);
    assert_eq!(
        git_lines.len(),
        grit_lines.len(),
        "line count for -L {range_spec:?}"
    );
    for (g, r) in git_lines.iter().zip(grit_lines.iter()) {
        assert_eq!(g.final_line, r.final_line, "-L {range_spec}");
        assert_eq!(g.commit, r.commit, "commit at line {}", g.final_line);
        assert_eq!(
            g.original_line, r.original_line,
            "orig at line {}",
            g.final_line
        );
    }
}

fn open_repo(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repo")
}

fn head_oid(dir: &Path) -> ObjectId {
    git_output(dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("HEAD oid")
}

#[test]
fn blame_line_range_numeric_relative_and_open_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git_run(root, &["init", "-q"]);
    git_run(root, &["config", "user.email", "blame-range@example.com"]);
    std::fs::write(root.join("story.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    git_run(root, &["add", "story.txt"]);
    git_run(root, &["commit", "-m", "add story", "-q"]);
    std::fs::write(root.join("story.txt"), "ONE\ntwo\nthree\nfour\nfive\n").unwrap();
    git_run(root, &["add", "story.txt"]);
    git_run(root, &["commit", "-m", "edit first", "-q"]);

    let repo = open_repo(root);
    let head = head_oid(root);

    for spec in ["2,+2", "4,-2", "3", "2,4"] {
        assert_range_matches_git(&repo, root, "story.txt", head, spec);
    }
}

#[test]
fn blame_line_range_regex_forms() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git_run(root, &["init", "-q"]);
    git_run(root, &["config", "user.email", "blame-range@example.com"]);
    std::fs::write(root.join("mix.txt"), "aaa\nbbb\nccc\n").unwrap();
    git_run(root, &["add", "mix.txt"]);
    git_run(root, &["commit", "-m", "add", "-q"]);

    let repo = open_repo(root);
    let head = head_oid(root);
    for spec in ["/bbb/", "/bbb/,+1", "^/bbb/"] {
        assert_range_matches_git(&repo, root, "mix.txt", head, spec);
    }
}

#[test]
fn blame_line_range_funcname_c_and_rust() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git_run(root, &["init", "-q"]);
    git_run(root, &["config", "user.email", "blame-range@example.com"]);
    std::fs::write(
        root.join("sample.c"),
        "#include <stdio.h>\nvoid foo(void) {\n  puts(\"a\");\n}\nvoid bar(void) {\n  puts(\"b\");\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("sample.rs"),
        "fn alpha() {\n    println!(\"a\");\n}\nfn beta() {\n    println!(\"b\");\n}\n",
    )
    .unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-m", "sources", "-q"]);

    let repo = open_repo(root);
    let head = head_oid(root);
    assert_range_matches_git(&repo, root, "sample.c", head, ":foo");
    assert_range_matches_git(&repo, root, "sample.rs", head, ":alpha");
}

#[test]
fn blame_line_range_custom_xfuncname_driver() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git_run(root, &["init", "-q"]);
    git_run(root, &["config", "user.email", "blame-range@example.com"]);
    std::fs::write(root.join(".gitattributes"), "probe.txt diff=probe\n").unwrap();
    git_run(
        root,
        &["config", "diff.probe.xfuncname", "^PROBE[[:space:]]+"],
    );
    std::fs::write(
        root.join("probe.txt"),
        "noise\nPROBE one\nline\nPROBE two\n",
    )
    .unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-m", "probe", "-q"]);

    let repo = open_repo(root);
    let head = head_oid(root);
    assert_range_matches_git(&repo, root, "probe.txt", head, ":one");
}

#[test]
fn blame_line_range_parser_errors() {
    use grit_lib::blame::LineRangeError;
    use grit_lib::config::ConfigSet;
    use grit_lib::line_range::resolve_line_range;

    let content = "a\nb\n";
    let cfg = ConfigSet::default();
    let err = resolve_line_range("0,1", content, "f", &cfg, None, 1).unwrap_err();
    assert!(matches!(err, LineRangeError::InvalidLineNumber { .. }));
    let err = resolve_line_range("/nomatch/", content, "f", &cfg, None, 1).unwrap_err();
    assert!(matches!(err, LineRangeError::RegexNoMatch { .. }));
    let err = resolve_line_range("9,9", content, "f", &cfg, None, 1).unwrap_err();
    assert!(matches!(err, LineRangeError::OutOfRange { .. }));
}
