//! Custom merge drivers (`merge.<name>.driver`, `merge` attributes) vs system `git`.

mod support;

use std::fs;

use support::git as hist_git;
use support::{
    assert_merge_commits_match_git, commit_file, open_grit_repo, skip_merge_tree_git,
    CommitMergeLabels, HistoryRepo,
};

fn init_repo() -> HistoryRepo {
    HistoryRepo::init()
}

/// Issue #1966 reproduction: custom driver and `merge=ours` with `driver=true`.
#[test]
fn custom_merge_drivers_match_git_merge() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    fs::write(
        history.worktree().join(".gitattributes"),
        "conf.cfg merge=keep\n*.lock merge=ours\n",
    )
    .unwrap();
    hist_git(
        history.worktree(),
        &[
            "config",
            "merge.keep.driver",
            "printf 'merged-by-driver\\n' > %A",
        ],
    );
    hist_git(history.worktree(), &["config", "merge.ours.driver", "true"]);
    hist_git(history.worktree(), &["add", ".gitattributes"]);
    fs::write(history.worktree().join("conf.cfg"), b"one\ntwo\n").unwrap();
    fs::write(history.worktree().join("a.lock"), b"l1\nl2\n").unwrap();
    hist_git(history.worktree(), &["add", "conf.cfg", "a.lock"]);
    hist_git(history.worktree(), &["commit", "-m", "base"]);
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "-c", "side"]);
    fs::write(history.worktree().join("conf.cfg"), b"one\ntwo\nside\n").unwrap();
    fs::write(history.worktree().join("a.lock"), b"l1\nl2\nside\n").unwrap();
    hist_git(history.worktree(), &["add", "conf.cfg", "a.lock"]);
    hist_git(history.worktree(), &["commit", "-m", "side"]);

    hist_git(history.worktree(), &["switch", "main"]);
    fs::write(history.worktree().join("conf.cfg"), b"one\ntwo\nmain\n").unwrap();
    fs::write(history.worktree().join("a.lock"), b"l1\nl2\nmain\n").unwrap();
    hist_git(history.worktree(), &["add", "conf.cfg", "a.lock"]);
    hist_git(history.worktree(), &["commit", "-m", "main"]);

    let grit = open_grit_repo(history.worktree());
    let ours: grit_lib::objects::ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: grit_lib::objects::ObjectId = hist_git(history.worktree(), &["rev-parse", "side"])
        .trim()
        .parse()
        .unwrap();
    let labels = CommitMergeLabels::from_commits(&ours, &theirs);
    assert_merge_commits_match_git(
        &history,
        &grit,
        &base,
        &ours,
        &theirs,
        labels.presentation(),
        "custom merge drivers #1966",
    );
}

/// Upstream t6406-style marker size placeholder (`%L`) with a custom driver.
#[test]
fn custom_driver_conflict_marker_size_placeholder() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    fs::write(
        history.worktree().join(".gitattributes"),
        "f.txt merge=custom conflict-marker-size=5\n",
    )
    .unwrap();
    hist_git(
        history.worktree(),
        &["config", "merge.custom.driver", "printf '%s\\n' %L > %A"],
    );
    hist_git(history.worktree(), &["add", ".gitattributes"]);
    commit_file(history.worktree(), "f.txt", "base\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "-c", "theirs"]);
    commit_file(history.worktree(), "f.txt", "base\ntheirs\n", "theirs");

    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(history.worktree(), "f.txt", "base\nours\n", "ours");

    let grit = open_grit_repo(history.worktree());
    let ours: grit_lib::objects::ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: grit_lib::objects::ObjectId =
        hist_git(history.worktree(), &["rev-parse", "theirs"])
            .trim()
            .parse()
            .unwrap();
    let labels = CommitMergeLabels::from_commits(&ours, &theirs);
    assert_merge_commits_match_git(
        &history,
        &grit,
        &base,
        &ours,
        &theirs,
        labels.presentation(),
        "custom driver %L marker size",
    );
}
