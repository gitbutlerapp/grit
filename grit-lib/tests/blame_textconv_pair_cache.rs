//! Regression: pair-map cache must include path/mode when textconv differs per path.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use grit_lib::blame::{
    compute_blame, parse_diff_algorithm_name, BlameDiffAlgorithm, BlameTextconvContext,
};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::load_graft_parents;

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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineAttr {
    final_line: usize,
    commit: ObjectId,
    original_line: usize,
}

fn parse_git_blame_porcelain(output: &str) -> Vec<LineAttr> {
    let mut lines = Vec::new();
    let mut group: Option<(ObjectId, usize, usize)> = None;
    for raw in output.lines() {
        if raw.starts_with('\t') {
            let Some((commit, orig, final_line)) = group else {
                continue;
            };
            lines.push(LineAttr {
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
        if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
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

fn build_textconv_copy_fixture() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_run(root, &["init", "-q", "-b", "main"]);
    git_run(root, &["config", "user.name", "Blame Test"]);
    git_run(root, &["config", "user.email", "blame@example.com"]);
    std::fs::write(root.join(".gitattributes"), "source.txt diff=sorted\n").unwrap();
    git_run(root, &["config", "diff.sorted.textconv", "sort"]);

    // Three revisions; source and target share blob oids at each commit (same bytes).
    std::fs::write(root.join("source.txt"), "delta\nbravo\nalpha\ncharlie\n").unwrap();
    std::fs::write(root.join("target.txt"), "delta\nbravo\nalpha\ncharlie\n").unwrap();
    git_run(root, &["add", "."]);
    git_run(root, &["commit", "-m", "c0"]);

    std::fs::write(
        root.join("source.txt"),
        "delta\nbravo\nalpha\ncharlie\necho\n",
    )
    .unwrap();
    std::fs::write(
        root.join("target.txt"),
        "delta\nbravo\nalpha\ncharlie\necho\n",
    )
    .unwrap();
    git_run(root, &["add", "source.txt", "target.txt"]);
    git_run(root, &["commit", "-m", "c1"]);

    std::fs::write(
        root.join("source.txt"),
        "delta\nbravo\nalpha\ncharlie\necho\nfoxtrot\n",
    )
    .unwrap();
    std::fs::write(
        root.join("target.txt"),
        "delta\nbravo\nalpha\ncharlie\necho\nfoxtrot\n",
    )
    .unwrap();
    git_run(root, &["add", "source.txt", "target.txt"]);
    git_run(root, &["commit", "-m", "c2"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open repo");
    (tmp, repo)
}

#[test]
fn blame_pair_map_cache_respects_textconv_path() {
    let (_tmp, repo) = build_textconv_copy_fixture();
    let dir = repo.work_tree().expect("worktree");
    let head = git_output(dir, &["rev-parse", "HEAD"])
        .trim()
        .parse::<ObjectId>()
        .expect("head oid");

    let git_lines = parse_git_blame_porcelain(&git_output(
        dir,
        &["blame", "--textconv", "-CCC", "--porcelain", "target.txt"],
    ));

    let config = repo.config().map(|c| (*c).clone()).unwrap_or_default();
    let diff_algorithm = config
        .get("diff.algorithm")
        .as_ref()
        .and_then(|name| parse_diff_algorithm_name(name))
        .unwrap_or(BlameDiffAlgorithm::Myers);
    let indent_heuristic = config
        .get_bool("diff.indentHeuristic")
        .and_then(|v| v.ok())
        .unwrap_or(false);
    let grafts = load_graft_parents(repo.git_dir());
    let ctx = BlameTextconvContext::new(&repo);

    let grit_lines = compute_blame(
        repo.odb(),
        head,
        "target.txt",
        &HashSet::new(),
        diff_algorithm,
        Some(&ctx),
        true,
        3,
        false,
        false,
        &grafts,
        indent_heuristic,
        None,
        grit_lib::diff::WhitespaceMode::default(),
        &grit_lib::parallel::ThreadPolicy::default(),
    )
    .expect("compute_blame");

    let grit_attrs: Vec<LineAttr> = grit_lines
        .into_iter()
        .map(|line| LineAttr {
            final_line: line.final_lineno,
            commit: line.oid,
            original_line: line.orig_lineno,
        })
        .collect();

    assert_eq!(
        git_lines.len(),
        grit_attrs.len(),
        "line count mismatch vs git --textconv -CCC"
    );
    for (git_line, grit_line) in git_lines.iter().zip(grit_attrs.iter()) {
        assert_eq!(git_line.final_line, grit_line.final_line);
        assert_eq!(
            git_line.commit, grit_line.commit,
            "commit at final line {}",
            git_line.final_line
        );
        assert_eq!(
            git_line.original_line, grit_line.original_line,
            "original line at final {}",
            git_line.final_line
        );
    }
}
