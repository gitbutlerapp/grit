//! `repo.diff()` metadata diffs match the system `git` raw output for each `TreeDiffOptions`
//! pairing, pathspec and tree-entry setting.

use crate::support;

use std::fs;

use grit_lib::diff::{BreakRewrites, CopyDetection, RenameDetection, TreeDiffOptions};
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags};

use support::{
    assert_raw_matches_git, commits_diff_raw, git_quiet, git_raw_lines, grit_raw_lines,
    index_to_tree_raw, index_to_worktree_raw, rev_parse_tree, tree_diff_raw,
    tree_to_worktree_raw_abbrev, DiffRepo,
};

fn numbered(prefix: &str, edit_line: Option<usize>) -> String {
    (1..=24)
        .map(|i| {
            if Some(i) == edit_line {
                format!("{prefix} line {i} edited\n")
            } else {
                format!("{prefix} line {i}\n")
            }
        })
        .collect()
}

/// Two commits: `a.txt` moves with an edit, `dir/b.txt` changes, `keep.txt` gets an unmodified
/// copy, and `gone.txt` is deleted.
fn history() -> DiffRepo {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    fs::create_dir_all(wt.join("dir")).unwrap();
    fs::write(wt.join("a.txt"), numbered("alpha", None)).unwrap();
    fs::write(wt.join("dir/b.txt"), numbered("beta", None)).unwrap();
    fs::write(wt.join("keep.txt"), numbered("keep", None)).unwrap();
    fs::write(wt.join("gone.txt"), "bye\n").unwrap();
    git_quiet(wt, &["add", "."]);
    git_quiet(wt, &["commit", "-qm", "base"]);
    git_quiet(wt, &["mv", "a.txt", "moved.txt"]);
    fs::write(wt.join("moved.txt"), numbered("alpha", Some(12))).unwrap();
    fs::write(wt.join("dir/b.txt"), numbered("beta", Some(3))).unwrap();
    fs::write(wt.join("keep-copy.txt"), numbered("keep", None)).unwrap();
    git_quiet(wt, &["rm", "-q", "gone.txt"]);
    git_quiet(wt, &["add", "-A"]);
    git_quiet(wt, &["commit", "-qm", "tip"]);
    dr
}

fn scoped(specs: &[&str]) -> Pathspec {
    Pathspec::scoped(specs, PathspecGlobalFlags::default())
}

/// `(git flags, options)` pairs every metadata diff is checked under.
fn pairings() -> Vec<(Vec<&'static str>, TreeDiffOptions)> {
    let renames = TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50));
    vec![
        (vec!["--no-renames"], TreeDiffOptions::default()),
        (vec!["-M50"], renames.clone()),
        (
            vec!["-M100%"],
            TreeDiffOptions::default().with_renames(RenameDetection::Similarity(100)),
        ),
        (
            vec!["-M50", "-C50"],
            renames.clone().with_copies(CopyDetection::Similarity(50)),
        ),
        (
            vec!["-M50", "-C50", "--find-copies-harder"],
            renames.clone().with_copies(CopyDetection::FindHarder(50)),
        ),
        (
            vec!["-B", "-M50"],
            renames.with_break_rewrites(BreakRewrites::DEFAULT),
        ),
    ]
}

#[test]
fn tree_to_tree_and_commits_match_git_diff_tree() {
    let dr = history();
    let wt = &dr.worktree;
    let base = rev_parse_tree(wt, "HEAD~1^{tree}");
    let tip = rev_parse_tree(wt, "HEAD^{tree}");
    let base_commit = rev_parse_tree(wt, "HEAD~1");
    let tip_commit = rev_parse_tree(wt, "HEAD");
    for (flags, options) in pairings() {
        let mut args = vec!["diff-tree", "-r", "--raw"];
        args.extend(&flags);
        let (b, t) = (base.to_hex(), tip.to_hex());
        args.extend([b.as_str(), t.as_str()]);
        let git = git_raw_lines(wt, &args);
        let context = format!("diff-tree {flags:?}");
        assert_raw_matches_git(
            &tree_diff_raw(&dr.repo, Some(&base), Some(&tip), &options),
            &git,
            &context,
        );
        assert_raw_matches_git(
            &commits_diff_raw(&dr.repo, &base_commit, &tip_commit, &options),
            &git,
            &format!("{context} on commits"),
        );
    }
}

#[test]
fn tree_to_tree_pathspec_and_show_tree_entries_match_git() {
    let dr = history();
    let wt = &dr.worktree;
    let base = rev_parse_tree(wt, "HEAD~1^{tree}");
    let tip = rev_parse_tree(wt, "HEAD^{tree}");
    let (b, t) = (base.to_hex(), tip.to_hex());

    let git = git_raw_lines(
        wt,
        &["diff-tree", "-r", "--raw", "-M50", &b, &t, "--", "dir"],
    );
    let options = TreeDiffOptions::default()
        .with_renames(RenameDetection::Similarity(50))
        .with_pathspec(Some(scoped(&["dir"])));
    assert_raw_matches_git(
        &tree_diff_raw(&dr.repo, Some(&base), Some(&tip), &options),
        &git,
        "diff-tree -- dir",
    );

    let git = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "--raw",
            "--no-renames",
            &b,
            &t,
            "--",
            "*.txt",
        ],
    );
    let options = TreeDiffOptions::default().with_pathspec(Some(scoped(&["*.txt"])));
    assert_raw_matches_git(
        &tree_diff_raw(&dr.repo, Some(&base), Some(&tip), &options),
        &git,
        "diff-tree -- '*.txt'",
    );

    let git = git_raw_lines(
        wt,
        &["diff-tree", "-r", "-t", "--raw", "--no-renames", &b, &t],
    );
    let options = TreeDiffOptions::default().with_show_tree_entries(true);
    assert_raw_matches_git(
        &tree_diff_raw(&dr.repo, Some(&base), Some(&tip), &options),
        &git,
        "diff-tree -t",
    );
}

#[test]
fn index_to_tree_matches_git_diff_index_cached() {
    let dr = history();
    let wt = &dr.worktree;
    let base = rev_parse_tree(wt, "HEAD~1^{tree}");
    let index = dr.load_index();
    for (flags, options) in pairings() {
        let mut args = vec!["diff-index", "-r", "--cached", "--raw"];
        args.extend(&flags);
        let b = base.to_hex();
        args.push(b.as_str());
        let git = git_raw_lines(wt, &args);
        assert_raw_matches_git(
            &index_to_tree_raw(&dr.repo, &index, &base, &options),
            &git,
            &format!("diff-index --cached {flags:?}"),
        );
    }
}

#[test]
fn index_to_worktree_matches_git_diff_files() {
    let dr = history();
    let wt = &dr.worktree;
    fs::write(wt.join("dir/b.txt"), numbered("beta", Some(20))).unwrap();
    fs::write(wt.join("keep.txt"), numbered("rewritten keep", None)).unwrap();
    fs::remove_file(wt.join("keep-copy.txt")).unwrap();
    for (flags, options) in [
        (vec!["--no-renames"], TreeDiffOptions::default()),
        (
            vec!["-M50"],
            TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50)),
        ),
    ] {
        let mut args = vec!["diff-files", "-r", "--raw"];
        args.extend(&flags);
        let git = git_raw_lines(wt, &args);
        let mut index = dr.load_index();
        assert_raw_matches_git(
            &index_to_worktree_raw(&dr.repo, &mut index, &options),
            &git,
            &format!("diff-files {flags:?}"),
        );
    }
    let git = git_raw_lines(wt, &["diff-files", "-r", "--raw", "--", "dir"]);
    let mut index = dr.load_index();
    let options = TreeDiffOptions::default().with_pathspec(Some(scoped(&["dir"])));
    assert_raw_matches_git(
        &index_to_worktree_raw(&dr.repo, &mut index, &options),
        &git,
        "diff-files -- dir",
    );
}

#[test]
fn tree_to_worktree_matches_git_diff_tree_against_worktree() {
    let dr = history();
    let wt = &dr.worktree;
    fs::write(wt.join("dir/b.txt"), numbered("beta", Some(20))).unwrap();
    fs::write(wt.join("extra.txt"), "staged\n").unwrap();
    git_quiet(wt, &["add", "extra.txt"]);
    let base = rev_parse_tree(wt, "HEAD~1^{tree}");
    let index = dr.load_index();
    let b = base.to_hex();
    for (flags, options) in [
        (vec!["--no-renames"], TreeDiffOptions::default()),
        (
            vec!["-M50"],
            TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50)),
        ),
        (
            vec!["-M50", "-C50", "--find-copies-harder"],
            TreeDiffOptions::default()
                .with_renames(RenameDetection::Similarity(50))
                .with_copies(CopyDetection::FindHarder(50)),
        ),
    ] {
        let mut args = vec!["diff", "-r", "--raw", "--abbrev=7"];
        args.extend(&flags);
        args.push(b.as_str());
        let git = git_raw_lines(wt, &args);
        assert_raw_matches_git(
            &tree_to_worktree_raw_abbrev(&dr.repo, &index, Some(&base), &options, 7),
            &git,
            &format!("diff {flags:?} <tree>"),
        );
    }
}

#[test]
fn detect_renames_matches_git_on_a_raw_list() {
    let dr = history();
    let wt = &dr.worktree;
    let base = rev_parse_tree(wt, "HEAD~1^{tree}");
    let tip = rev_parse_tree(wt, "HEAD^{tree}");
    let raw = dr
        .repo
        .diff()
        .tree_to_tree(Some(&base), Some(&tip), &TreeDiffOptions::default())
        .expect("raw")
        .entries;
    let options = TreeDiffOptions::default()
        .with_renames(RenameDetection::Similarity(50))
        .with_copies(CopyDetection::FindHarder(50));
    let paired = dr
        .repo
        .diff()
        .detect_renames(raw, Some(&base), &options)
        .expect("pair")
        .entries;
    let (b, t) = (base.to_hex(), tip.to_hex());
    let git = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "-r",
            "--raw",
            "-M50",
            "-C50",
            "--find-copies-harder",
            &b,
            &t,
        ],
    );
    assert_raw_matches_git(
        &grit_raw_lines(&paired),
        &git,
        "detect_renames -C --find-copies-harder",
    );
}
