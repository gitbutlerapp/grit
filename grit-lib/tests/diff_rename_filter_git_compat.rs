//! Rename thresholds, `--no-renames`, and `--diff-filter` vs system `git diff`.

use crate::support;

use grit_lib::diff::{
    apply_diff_filter, diff_trees_with_renames, DiffEntry, DiffStatus, InvalidDiffFilter,
    RenameDetection,
};
use grit_lib::objects::ObjectId;
use std::fs;

use support::{git_output, git_quiet, rev_parse_tree, DiffRepo};

fn git_name_status(dir: &std::path::Path, args: &[&str]) -> Vec<String> {
    let mut cmd_args = vec!["diff", "--name-status"];
    cmd_args.extend(args);
    let out = git_output(dir, &cmd_args);
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

fn grit_name_status(entries: &[DiffEntry]) -> Vec<String> {
    let mut lines: Vec<String> = entries
        .iter()
        .map(|e| match e.status {
            DiffStatus::Renamed => format!(
                "R{:03}\t{}\t{}",
                e.score.unwrap_or(100),
                e.old_path.as_deref().unwrap_or(""),
                e.new_path.as_deref().unwrap_or("")
            ),
            DiffStatus::Added => format!("A\t{}", e.path()),
            DiffStatus::Deleted => format!("D\t{}", e.path()),
            DiffStatus::Modified => format!("M\t{}", e.path()),
            DiffStatus::TypeChanged => format!("T\t{}", e.path()),
            DiffStatus::Copied => format!(
                "C{:03}\t{}\t{}",
                e.score.unwrap_or(100),
                e.old_path.as_deref().unwrap_or(""),
                e.new_path.as_deref().unwrap_or("")
            ),
            DiffStatus::Unmerged => format!("U\t{}", e.path()),
        })
        .collect();
    lines.sort();
    lines
}

fn t4001_rename_repo() -> (DiffRepo, ObjectId, ObjectId) {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    let path0: String = (1..=15)
        .map(|i| {
            if i == 11 {
                "line 11".to_string()
            } else {
                format!("Line {i}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(wt.join("path0"), format!("{path0}\n")).unwrap();
    git_quiet(wt, &["add", "path0"]);
    git_quiet(wt, &["commit", "-qm", "base"]);
    let parent = rev_parse_tree(wt, "HEAD^{tree}");

    let edited = path0.replace("line 11", "Line 11");
    fs::remove_file(wt.join("path0")).unwrap();
    fs::write(wt.join("path1"), format!("{edited}\n")).unwrap();
    git_quiet(wt, &["add", "-A"]);
    git_quiet(wt, &["commit", "-qm", "rename"]);
    let commit = rev_parse_tree(wt, "HEAD^{tree}");
    (dr, parent, commit)
}

#[test]
fn rename_threshold_matches_git_m() {
    let (dr, parent, commit) = t4001_rename_repo();
    let wt = &dr.worktree;
    for threshold in [30u32, 50, 90, 100] {
        let git_m = if threshold == 100 {
            "-M100%".to_string()
        } else {
            format!("-M{threshold}")
        };
        let git_lines = git_name_status(wt, &[&git_m, &parent.to_hex(), &commit.to_hex()]);
        let grit_entries =
            diff_trees_with_renames(dr.repo.odb(), Some(&parent), Some(&commit), "", threshold)
                .expect("diff");
        let grit_lines = grit_name_status(&grit_entries);
        assert_eq!(
            git_lines, grit_lines,
            "threshold {threshold}: grit name-status must match git"
        );
    }
}

#[test]
fn no_renames_reports_add_and_delete() {
    let (dr, parent, commit) = t4001_rename_repo();
    let wt = &dr.worktree;
    let git_lines = git_name_status(wt, &["--no-renames", &parent.to_hex(), &commit.to_hex()]);
    let grit_entries = diff_trees_with_renames(
        dr.repo.odb(),
        Some(&parent),
        Some(&commit),
        "",
        grit_lib::diff::RENAME_DETECTION_DISABLED,
    )
    .expect("diff");
    let grit_lines = grit_name_status(&grit_entries);
    assert_eq!(git_lines, grit_lines);
    assert!(
        grit_lines.iter().any(|l| l.starts_with("A\t")),
        "expected added path without renames"
    );
    assert!(
        grit_lines.iter().any(|l| l.starts_with("D\t")),
        "expected deleted path without renames"
    );
    assert!(
        !grit_lines.iter().any(|l| l.starts_with('R')),
        "rename line must not appear"
    );
}

#[test]
fn diff_filter_matches_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path();
    git_quiet(wt, &["init", "-q", "-b", "main"]);
    fs::write(wt.join("keep.txt"), "same\n").unwrap();
    fs::write(wt.join("gone.txt"), "old\n").unwrap();
    git_quiet(wt, &["add", "."]);
    git_quiet(wt, &["commit", "-qm", "base"]);
    let base = rev_parse_tree(wt, "HEAD^{tree}");
    fs::remove_file(wt.join("gone.txt")).unwrap();
    fs::write(wt.join("new.txt"), "new\n").unwrap();
    fs::write(wt.join("keep.txt"), "changed\n").unwrap();
    git_quiet(wt, &["add", "-A"]);
    git_quiet(wt, &["commit", "-qm", "mix"]);
    let tip = rev_parse_tree(wt, "HEAD^{tree}");

    let repo = grit_lib::repo::Repository::discover(
        Some(wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open");

    for spec in ["A", "D", "M", "AD", "a", "d", "aD", "AM"] {
        let git_lines =
            git_name_status(wt, &["--diff-filter", spec, &base.to_hex(), &tip.to_hex()]);
        let raw =
            diff_trees_with_renames(repo.odb(), Some(&base), Some(&tip), "", 50).expect("diff");
        let filter: grit_lib::diff::DiffFilter = spec.parse().expect("filter");
        let filtered = apply_diff_filter(raw, Some(filter));
        let grit_lines = grit_name_status(&filtered);
        assert_eq!(git_lines, grit_lines, "filter={spec}");
    }
}

#[test]
fn diff_filter_rejects_unknown_letters() {
    assert!(matches!(
        "C".parse::<grit_lib::diff::DiffFilter>(),
        Err(InvalidDiffFilter::Unsupported('C'))
    ));
    assert!(matches!(
        "z".parse::<grit_lib::diff::DiffFilter>(),
        Err(InvalidDiffFilter::Unknown('z'))
    ));
}

#[test]
fn algorithm_flag_overrides_config() {
    use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
    use grit_lib::diff::{DiffAlgorithm, DiffOptions};

    let file = ConfigFile::parse(
        std::path::Path::new("test"),
        "[diff]\n\talgorithm = patience\n",
        ConfigScope::Local,
    )
    .expect("parse");
    let mut set = ConfigSet::new();
    set.merge(&file);
    let mut opts = DiffOptions::from_config(&set);
    assert_eq!(opts.algorithm, DiffAlgorithm::Patience);
    opts.algorithm = DiffAlgorithm::Histogram;
    assert_eq!(opts.algorithm, DiffAlgorithm::Histogram);

    let opts_off = opts.with_renames(RenameDetection::Off);
    assert_eq!(opts_off.clone().rename_similarity_threshold(), None);
    assert_eq!(
        opts_off.rename_threshold_u32(),
        grit_lib::diff::RENAME_DETECTION_DISABLED
    );
}

#[test]
fn exact_rename_matches_git_m100_percent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path();
    git_quiet(wt, &["init", "-q", "-b", "main"]);
    std::fs::write(wt.join("old.txt"), "same content\n").unwrap();
    git_quiet(wt, &["add", "old.txt"]);
    git_quiet(wt, &["commit", "-qm", "base"]);
    let parent = rev_parse_tree(wt, "HEAD^{tree}");
    git_quiet(wt, &["mv", "old.txt", "new.txt"]);
    git_quiet(wt, &["commit", "-qm", "rename"]);
    let commit = rev_parse_tree(wt, "HEAD^{tree}");

    let git_lines = git_name_status(wt, &["-M100%", &parent.to_hex(), &commit.to_hex()]);
    assert!(
        git_lines
            .iter()
            .any(|l| l.starts_with("R100\told.txt\tnew.txt")),
        "git: {git_lines:?}"
    );

    let repo = grit_lib::repo::Repository::discover(
        Some(wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("open");
    let grit_entries =
        diff_trees_with_renames(repo.odb(), Some(&parent), Some(&commit), "", 100).expect("diff");
    let grit_lines = grit_name_status(&grit_entries);
    assert_eq!(git_lines, grit_lines);
}
