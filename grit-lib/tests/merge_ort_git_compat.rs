//! Three-way tree merge vs `git merge-tree --write-tree` (merge-ort oracle).

use crate::support;

use std::fs;
use std::os::unix::fs::symlink;

use grit_lib::objects::ObjectId;
use grit_test_support::git;

use support::git as hist_git;
use support::{
    assert_merge_commits_match_git, commit_file, commit_tree, grit_merge_result_tree,
    open_grit_repo, skip_merge_tree_git, CommitMergeLabels, HistoryRepo,
};

#[test]
fn t6406_merge_drivers_match_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    fs::write(
        history.worktree().join(".gitattributes"),
        "*.bin binary\nunion.txt merge=union\n",
    )
    .unwrap();
    hist_git(history.worktree(), &["add", ".gitattributes"]);
    fs::write(history.worktree().join("f.bin"), b"v1").unwrap();
    fs::write(history.worktree().join("union.txt"), b"a\nb\nc\n").unwrap();
    hist_git(history.worktree(), &["add", "f.bin", "union.txt"]);
    hist_git(history.worktree(), &["commit", "-m", "base"]);
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "-c", "driver-theirs"]);
    fs::write(history.worktree().join("f.bin"), b"v2").unwrap();
    fs::write(history.worktree().join("union.txt"), b"a\nB\nc\n").unwrap();
    hist_git(history.worktree(), &["add", "f.bin", "union.txt"]);
    hist_git(history.worktree(), &["commit", "-m", "theirs"]);

    hist_git(history.worktree(), &["switch", "main"]);
    fs::write(history.worktree().join("f.bin"), b"v3").unwrap();
    fs::write(history.worktree().join("union.txt"), b"a\nX\nc\n").unwrap();
    hist_git(history.worktree(), &["add", "f.bin", "union.txt"]);
    hist_git(history.worktree(), &["commit", "-m", "ours"]);

    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "driver-theirs"])
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
        "t6406 binary attribute conflict",
    );
}

#[test]
fn add_add_conflict_matches_git_merge_tree() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "base.txt", "base\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    hist_git(history.worktree(), &["switch", "-c", "add-theirs"]);
    commit_file(history.worktree(), "new.txt", "theirs\n", "theirs add");
    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(history.worktree(), "new.txt", "ours\n", "ours add");
    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "add-theirs"])
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
        "add/add conflict",
    );
}

#[test]
fn modify_delete_conflict_matches_git_merge_tree() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "f.txt", "base\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    hist_git(history.worktree(), &["switch", "-c", "del"]);
    hist_git(history.worktree(), &["rm", "f.txt"]);
    hist_git(history.worktree(), &["commit", "-m", "delete"]);
    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(history.worktree(), "f.txt", "ours\n", "modify");
    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "del"])
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
        "modify/delete",
    );
}

#[test]
fn content_conflict_markers_match_git_merge_tree() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "g.txt", "1\n2\n3\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    hist_git(history.worktree(), &["switch", "-c", "side"]);
    commit_file(history.worktree(), "g.txt", "1\nSIDE\n3\n", "theirs");
    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(history.worktree(), "g.txt", "1\nOURS\n3\n", "ours");
    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "side"])
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
        "content conflict markers",
    );
}

fn init_repo() -> HistoryRepo {
    let repo = HistoryRepo::init();
    hist_git(repo.worktree(), &["config", "core.autocrlf", "false"]);
    repo
}

#[test]
fn t6400_df_conflicts_match_git_merge_tree() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    let base = commit_file(history.worktree(), "init", "Hello\n", "initial");
    hist_git(history.worktree(), &["switch", "-c", "B"]);
    commit_file(history.worktree(), "dir/foo", "foo\n", "file dir/foo");
    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(history.worktree(), "dir", "file dir\n", "file dir");

    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "B"])
        .trim()
        .parse()
        .expect("B");
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .expect("main");
    let labels = CommitMergeLabels::from_commits(&ours, &theirs);
    assert_merge_commits_match_git(
        &history,
        &grit,
        &base,
        &ours,
        &theirs,
        labels.presentation(),
        "t6400 d/f conflict",
    );
}

#[test]
fn t6402_rename_merges_match_git_merge_tree() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "old.txt", "content\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    hist_git(history.worktree(), &["switch", "-c", "rename-theirs"]);
    hist_git(history.worktree(), &["mv", "old.txt", "new.txt"]);
    commit_file(
        history.worktree(),
        "new.txt",
        "content\n",
        "rename on theirs",
    );
    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(
        history.worktree(),
        "old.txt",
        "content\nchanged\n",
        "edit on ours",
    );

    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "rename-theirs"])
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
        "t6402 rename + edit",
    );
}

#[test]
fn t6422_rename_corner_cases_match_git_merge_tree() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "shared.txt", "base\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    // rename/delete: ours renames, theirs deletes old path
    hist_git(history.worktree(), &["switch", "-c", "rd-theirs"]);
    hist_git(history.worktree(), &["rm", "shared.txt"]);
    commit_file(history.worktree(), "other.txt", "x\n", "theirs delete");

    hist_git(history.worktree(), &["switch", "main"]);
    hist_git(history.worktree(), &["mv", "shared.txt", "renamed.txt"]);
    commit_file(history.worktree(), "renamed.txt", "base\n", "ours rename");

    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "rd-theirs"])
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
        "t6422 rename/delete",
    );
}

#[test]
fn t6411_filemode_merge_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    fs::write(history.worktree().join("script.sh"), "#!/bin/sh\n").unwrap();
    hist_git(history.worktree(), &["add", "script.sh"]);
    hist_git(
        history.worktree(),
        &["update-index", "--chmod=+x", "script.sh"],
    );
    hist_git(history.worktree(), &["commit", "-m", "base executable"]);
    let base: ObjectId = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    hist_git(history.worktree(), &["switch", "-c", "mode-theirs"]);
    hist_git(
        history.worktree(),
        &["commit", "--allow-empty", "-m", "theirs unchanged exec"],
    );
    hist_git(
        history.worktree(),
        &["branch", "-f", "main", &base.to_hex()],
    );
    hist_git(history.worktree(), &["switch", "-f", "main"]);
    hist_git(
        history.worktree(),
        &["update-index", "--chmod=-x", "script.sh"],
    );
    hist_git(history.worktree(), &["commit", "-m", "non-exec on ours"]);

    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "mode-theirs"])
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
        "t6411 mode add/add same content",
    );
}

#[test]
fn t6405_symlink_merge_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    symlink("target-a", history.worktree().join("link")).unwrap();
    hist_git(history.worktree(), &["add", "link"]);
    hist_git(history.worktree(), &["commit", "-m", "base symlink"]);
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "-c", "sym-theirs"]);
    fs::remove_file(history.worktree().join("link")).unwrap();
    symlink("target-b", history.worktree().join("link")).unwrap();
    hist_git(history.worktree(), &["add", "link"]);
    hist_git(history.worktree(), &["commit", "-m", "theirs symlink"]);

    hist_git(history.worktree(), &["switch", "main"]);
    fs::remove_file(history.worktree().join("link")).unwrap();
    symlink("target-c", history.worktree().join("link")).unwrap();
    hist_git(history.worktree(), &["add", "link"]);
    hist_git(history.worktree(), &["commit", "-m", "ours symlink"]);

    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "sym-theirs"])
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
        "t6405 symlink conflict",
    );
}

#[test]
fn t6407_binary_conflict_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    fs::write(history.worktree().join("bin"), [0, 1, 2]).unwrap();
    hist_git(history.worktree(), &["add", "bin"]);
    hist_git(history.worktree(), &["commit", "-m", "base"]);
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "-c", "bin-theirs"]);
    fs::write(history.worktree().join("bin"), [0, 1, 3]).unwrap();
    hist_git(history.worktree(), &["add", "bin"]);
    hist_git(history.worktree(), &["commit", "-m", "theirs"]);

    hist_git(history.worktree(), &["switch", "main"]);
    fs::write(history.worktree().join("bin"), [0, 1, 4]).unwrap();
    hist_git(history.worktree(), &["add", "bin"]);
    hist_git(history.worktree(), &["commit", "-m", "ours"]);

    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "bin-theirs"])
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
        "t6407 binary",
    );
}

#[test]
fn t4301_merge_tree_write_tree_clean_merge_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "a.txt", "base\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    hist_git(history.worktree(), &["switch", "-c", "side"]);
    commit_file(history.worktree(), "b.txt", "new\n", "add on side");
    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(
        history.worktree(),
        "a.txt",
        "base\nours edit\n",
        "edit on main",
    );

    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "side"])
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
        "t4301 clean merge",
    );
}

#[test]
fn ours_only_addition_persists_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "shared.txt", "base\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    hist_git(history.worktree(), &["switch", "-c", "topic"]);
    commit_file(history.worktree(), "shared.txt", "topic\n", "topic");
    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(history.worktree(), "main-only.txt", "main\n", "main-only");
    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "topic"])
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
        "ours-only path kept",
    );
}

#[test]
fn t6417_merge_ours_theirs_favor_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "f.txt", "base\n", "base");
    let base = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    hist_git(history.worktree(), &["switch", "-c", "favor-theirs"]);
    commit_file(history.worktree(), "f.txt", "theirs\n", "theirs");
    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(history.worktree(), "f.txt", "ours\n", "ours");

    let grit = open_grit_repo(history.worktree());
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "main"])
        .trim()
        .parse()
        .unwrap();
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "favor-theirs"])
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
        "t6417 textual conflict default",
    );
}

#[test]
fn cherry_pick_tree_remap_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "on-main.txt", "main\n", "main");
    hist_git(history.worktree(), &["switch", "-c", "topic"]);
    commit_file(
        history.worktree(),
        "on-topic.txt",
        "topic\n",
        "topic commit",
    );
    let pick = hist_git(history.worktree(), &["rev-parse", "topic"])
        .trim()
        .to_owned();
    hist_git(history.worktree(), &["switch", "main"]);
    commit_file(
        history.worktree(),
        "on-main.txt",
        "main evolved\n",
        "main advance",
    );

    let grit = open_grit_repo(history.worktree());
    let pick_oid: ObjectId = pick.parse().unwrap();
    let main_oid: ObjectId = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let pick_parent = {
        let obj = grit.odb().read(&pick_oid).unwrap();
        grit_lib::objects::parse_commit(&obj.data).unwrap().parents[0]
    };
    let labels = CommitMergeLabels::from_commits(&main_oid, &pick_oid);
    assert_merge_commits_match_git(
        &history,
        &grit,
        &pick_parent,
        &main_oid,
        &pick_oid,
        labels.presentation(),
        "cherry-pick three-way tree",
    );

    let git_dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", history.worktree().display()),
            &git_dir.path().to_string_lossy(),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    git(git_dir.path(), &["cherry-pick", "--no-commit", &pick]);
    let git_tree = git(git_dir.path(), &["write-tree"]).trim().to_owned();
    let grit_tree = {
        let presentation = grit_lib::diff::merge_trees::TreeMergeConflictPresentation {
            df_directory_ours_commit: Some(main_oid),
            df_directory_theirs_commit: Some(pick_oid),
            ..labels.presentation()
        };
        let merged = grit_lib::diff::merge_trees::merge_trees_three_way(
            &grit,
            commit_tree(&grit, &pick_parent).unwrap(),
            commit_tree(&grit, &main_oid).unwrap(),
            commit_tree(&grit, &pick_oid).unwrap(),
            grit_lib::diff::merge_file::MergeFavor::default(),
            grit_lib::diff::merge_trees::WhitespaceMergeOptions::default(),
            None,
            presentation,
        )
        .unwrap();
        grit_merge_result_tree(&grit, &merged).unwrap().to_hex()
    };
    assert_eq!(grit_tree, git_tree, "cherry-pick --no-commit tree");
}

#[test]
fn clean_theirs_deletes_unchanged_blob_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "f.txt", "shared\n", "base");
    let base: ObjectId = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "-c", "theirs"]);
    hist_git(history.worktree(), &["rm", "f.txt"]);
    hist_git(history.worktree(), &["commit", "-m", "delete f"]);
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "main"]);
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let grit = open_grit_repo(history.worktree());
    let labels = CommitMergeLabels::from_commits(&ours, &theirs);
    assert_merge_commits_match_git(
        &history,
        &grit,
        &base,
        &ours,
        &theirs,
        labels.presentation(),
        "clean theirs-only deletion of unchanged blob",
    );
}

#[test]
fn nested_one_sided_directory_addition_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_repo();
    commit_file(history.worktree(), "src/a.txt", "a\n", "base");
    let base: ObjectId = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "-c", "theirs"]);
    std::fs::create_dir_all(history.worktree().join("src/newdir")).unwrap();
    fs::write(history.worktree().join("src/newdir/x.txt"), "x\n").unwrap();
    hist_git(history.worktree(), &["add", "src/newdir/x.txt"]);
    hist_git(history.worktree(), &["commit", "-m", "add nested file"]);
    let theirs: ObjectId = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    hist_git(history.worktree(), &["switch", "main"]);
    let ours: ObjectId = hist_git(history.worktree(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let grit = open_grit_repo(history.worktree());
    let labels = CommitMergeLabels::from_commits(&ours, &theirs);
    assert_merge_commits_match_git(
        &history,
        &grit,
        &base,
        &ours,
        &theirs,
        labels.presentation(),
        "nested directory added on theirs only",
    );
}
