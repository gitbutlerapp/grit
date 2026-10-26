//! Custom merge drivers (`merge.<name>.driver`, `merge` attributes) vs system `git`.

use crate::support;

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

/// t6406: non-zero driver exit must keep `%A` content for checkout / conflict blobs.
#[test]
fn custom_driver_nonzero_exit_keeps_result_blob() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    fs::write(
        history.worktree().join(".gitattributes"),
        "f.txt merge=custom\n",
    )
    .unwrap();
    hist_git(
        history.worktree(),
        &[
            "config",
            "merge.custom.driver",
            "printf 'driver-conflict\\n' > %A; exit 1",
        ],
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
    commit_file(history.worktree(), "f.txt", "base\nmain\n", "main");

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
        "custom driver nonzero exit keeps %A",
    );
}

/// `merge.default=union` applies when the path has no explicit merge attribute.
#[test]
fn merge_default_union_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    hist_git(history.worktree(), &["config", "merge.default", "union"]);
    commit_file(history.worktree(), "u.txt", "a\nb\nc\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "-c", "theirs"]);
    commit_file(history.worktree(), "u.txt", "a\nB\nc\n", "theirs");

    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(history.worktree(), "u.txt", "a\nX\nc\n", "ours");

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
        "merge.default=union",
    );
}

/// Add/add with a clean custom driver must still conflict on executable-bit mismatch (t6411).
#[test]
fn add_add_custom_driver_mode_clash_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    fs::write(
        history.worktree().join(".gitattributes"),
        "f.txt merge=custom\n",
    )
    .unwrap();
    hist_git(
        history.worktree(),
        &["config", "merge.custom.driver", "printf 'same\\n' > %A"],
    );
    hist_git(history.worktree(), &["add", ".gitattributes"]);
    commit_file(history.worktree(), "base.txt", "base\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "-c", "theirs"]);
    let theirs_path = history.worktree().join("f.txt");
    fs::write(&theirs_path, b"same\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&theirs_path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    hist_git(history.worktree(), &["add", "--chmod=+x", "f.txt"]);
    hist_git(history.worktree(), &["commit", "-m", "theirs add"]);

    hist_git(history.worktree(), &["switch", "main"]);
    fs::write(history.worktree().join("f.txt"), b"same\n").unwrap();
    hist_git(history.worktree(), &["add", "f.txt"]);
    hist_git(history.worktree(), &["commit", "-m", "ours add"]);

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
        "add/add driver clean but mode clash",
    );
}
