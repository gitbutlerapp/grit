//! Three-way tree merge vs `git merge-tree --write-tree` (merge-ort oracle).

use crate::support;

use std::fs;
use std::os::unix::fs::symlink;

use grit_lib::objects::ObjectId;
use grit_test_support::git;

use support::git as hist_git;
use support::{
    assert_merge_commits_match_git, commit_file, commit_tree, criss_cross_conflicting_resolutions,
    grit_merge_result_tree, open_grit_repo, skip_merge_tree_git, CommitMergeLabels, HistoryRepo,
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
            grit_lib::diff::merge_trees::DirectoryRenameMode::default(),
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

/// What a merge left behind: the merged tree, or the unmerged stages and conflicted files.
#[derive(Debug, PartialEq, Eq)]
enum MergeState {
    Merged {
        tree: String,
    },
    Conflicted {
        unmerged: String,
        files: Vec<(String, Option<String>)>,
    },
}

fn conflicted_state(root: &std::path::Path, paths: &[String]) -> MergeState {
    MergeState::Conflicted {
        unmerged: git(root, &["ls-files", "-u"]),
        files: paths
            .iter()
            .map(|p| {
                let content = fs::read(root.join(p))
                    .ok()
                    .map(|b| String::from_utf8_lossy(&b).into_owned());
                (p.clone(), content)
            })
            .collect(),
    }
}

/// Merge `theirs_branch` into the branch `ours_branch` with grit's `integrate_branch`, then
/// with `git merge` from the same starting point, and require the same outcome: the same
/// merged tree, or the same unmerged stages and conflicted work tree files. Both sides
/// compute their merge bases themselves, so several bases go through a virtual base.
fn assert_integrate_matches_git_merge(
    root: &std::path::Path,
    ours_branch: &str,
    theirs_branch: &str,
) -> MergeState {
    use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};

    hist_git(root, &["switch", "-q", "-f", ours_branch]);
    hist_git(root, &["reset", "-q", "--hard"]);
    let rev = |name: &str| -> ObjectId { git(root, &["rev-parse", name]).trim().parse().unwrap() };
    let (ours, theirs) = (rev(ours_branch), rev(theirs_branch));

    let repo = open_grit_repo(root);
    let outcome = integrate_branch(
        &repo,
        &format!("refs/heads/{ours_branch}"),
        ours,
        theirs,
        theirs_branch,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("integrate_branch");
    drop(repo);
    let (grit_state, conflicted) = match outcome {
        IntegrateBranchOutcome::Merged { oid, .. } => {
            let tree = git(root, &["rev-parse", &format!("{}^{{tree}}", oid.to_hex())]);
            git(root, &["fsck", "--strict"]);
            hist_git(root, &["reset", "-q", "--hard", &ours.to_hex()]);
            (MergeState::Merged { tree }, None)
        }
        IntegrateBranchOutcome::Conflicts { paths, .. } => {
            let state = conflicted_state(root, &paths);
            hist_git(root, &["merge", "--abort"]);
            (state, Some(paths))
        }
        other => panic!("{ours_branch} <- {theirs_branch}: unexpected outcome {other:?}"),
    };

    let git_merge = grit_test_support::git_cmd(&["merge", "-q", "--no-edit", theirs_branch])
        .in_dir(root)
        .exec();
    let git_state = match &conflicted {
        None => {
            assert!(
                git_merge.ok(),
                "git merge must succeed like grit: {}",
                git_merge.dump("git")
            );
            let tree = git(root, &["rev-parse", "HEAD^{tree}"]);
            hist_git(root, &["reset", "-q", "--hard", &ours.to_hex()]);
            MergeState::Merged { tree }
        }
        Some(paths) => {
            assert!(
                !git_merge.ok(),
                "git merge must stop on conflicts like grit"
            );
            let state = conflicted_state(root, paths);
            hist_git(root, &["merge", "--abort"]);
            state
        }
    };
    assert_eq!(
        grit_state, git_state,
        "{ours_branch} <- {theirs_branch}: grit and git merge must leave the same state"
    );
    grit_state
}

#[test]
fn criss_cross_conflicting_resolutions_integrate_matches_git_merge() {
    if !skip_merge_tree_git() {
        return;
    }
    let cc = criss_cross_conflicting_resolutions();
    let root = cc.dir.path();
    for (ours, theirs) in [("ours", "theirs"), ("theirs", "ours")] {
        let state = assert_integrate_matches_git_merge(root, ours, theirs);
        assert!(
            matches!(state, MergeState::Conflicted { .. }),
            "{ours} <- {theirs} must conflict"
        );
        hist_git(root, &["switch", "-q", ours]);
        let repo = open_grit_repo(root);
        let ours_oid: ObjectId = git(root, &["rev-parse", ours]).trim().parse().unwrap();
        let theirs_oid: ObjectId = git(root, &["rev-parse", theirs]).trim().parse().unwrap();
        grit_lib::porcelain::merge::integrate_branch(
            &repo,
            &format!("refs/heads/{ours}"),
            ours_oid,
            theirs_oid,
            theirs,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("integrate_branch");
        let stage1 = git(root, &["show", ":1:f"]);
        assert_eq!(
            stage1,
            "<<<<<<<<< Temporary merge branch 1\n2\n=========\n3\n>>>>>>>>> Temporary merge branch 2\n",
            "stage 1 must hold the virtual base with nested markers"
        );
        hist_git(root, &["merge", "--abort"]);
    }
}

#[test]
fn criss_cross_integrate_with_favor_matches_git_merge_tree() {
    if !skip_merge_tree_git() {
        return;
    }
    use grit_lib::diff::merge_file::MergeFavor;
    use grit_lib::porcelain::merge::{integrate_branch_with, IntegrateBranchOutcome, MergeOptions};

    for favor in [MergeFavor::Ours, MergeFavor::Theirs] {
        let cc = criss_cross_conflicting_resolutions();
        let root = cc.dir.path();
        let oracle = support::git_merge_tree_oracle(root, None, &cc.ours, &cc.theirs, favor);
        assert!(
            oracle.unmerged.is_empty(),
            "git resolves with -X: {}",
            oracle.stdout
        );
        let repo = open_grit_repo(root);
        let outcome = integrate_branch_with(
            &repo,
            "refs/heads/ours",
            cc.ours,
            cc.theirs,
            "theirs",
            &MergeOptions {
                favor,
                ..MergeOptions::default()
            },
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("integrate_branch_with");
        let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
            panic!("expected a merge commit with {favor:?}, got {outcome:?}");
        };
        let tree: ObjectId = git(root, &["rev-parse", &format!("{}^{{tree}}", oid.to_hex())])
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            tree, oracle.result_tree,
            "{favor:?} merge tree must match git"
        );
        git(root, &["fsck", "--strict"]);
    }
}

/// [`init_repo`] with a configured identity, which grit needs to record merge commits.
fn init_identity_repo() -> HistoryRepo {
    let repo = init_repo();
    hist_git(repo.worktree(), &["config", "user.email", "t@example.com"]);
    hist_git(repo.worktree(), &["config", "user.name", "Test"]);
    repo
}

/// Run `git` with explicit author and committer dates.
fn git_dated(root: &std::path::Path, author: &str, committer: &str, args: &[&str]) {
    grit_test_support::git_cmd(args)
        .in_dir(root)
        .env("GIT_AUTHOR_DATE", author)
        .env("GIT_COMMITTER_DATE", committer)
        .suc();
}

fn write_lines(root: &std::path::Path, path: &str, lines: &[&str]) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(full, body).unwrap();
}

/// Upstream t6401: a criss-cross merge whose bases merge cleanly into a virtual base.
#[test]
fn t6401_criss_cross_merge_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_identity_repo();
    let root = history.worktree();
    let mut step = 0u32;
    let mut commit = |args: &[&str]| {
        step += 1;
        let date = format!("{} +0000", 1_200_000_000 + step * 60);
        git_dated(root, &date, &date, args);
    };
    write_lines(root, "file", &["1", "2", "3", "4", "5", "6", "7", "8", "9"]);
    hist_git(root, &["add", "file"]);
    commit(&["commit", "-q", "-m", "Initial commit"]);
    hist_git(root, &["branch", "A"]);
    hist_git(root, &["branch", "B"]);
    hist_git(root, &["switch", "-q", "A"]);
    write_lines(
        root,
        "file",
        &[
            "1",
            "2",
            "3",
            "4",
            "5",
            "6",
            "7",
            "8 changed in B8, branch A",
            "9",
        ],
    );
    commit(&["commit", "-q", "-am", "B8"]);
    hist_git(root, &["switch", "-q", "B"]);
    write_lines(
        root,
        "file",
        &[
            "1",
            "2",
            "3 changed in C3, branch B",
            "4",
            "5",
            "6",
            "7",
            "8",
            "9",
        ],
    );
    commit(&["commit", "-q", "-am", "C3"]);
    hist_git(root, &["branch", "C3"]);
    commit(&["merge", "-q", "-m", "pre E3 merge", "A"]);
    write_lines(
        root,
        "file",
        &[
            "1",
            "2",
            "3 changed in E3, branch B. New file size",
            "4",
            "5",
            "6",
            "7",
            "8 changed in B8, branch A",
            "9",
        ],
    );
    commit(&["commit", "-q", "-am", "E3"]);
    hist_git(root, &["switch", "-q", "A"]);
    commit(&["merge", "-q", "-m", "pre D8 merge", "C3"]);
    write_lines(
        root,
        "file",
        &[
            "1",
            "2",
            "3 changed in C3, branch B",
            "4",
            "5",
            "6",
            "7",
            "8 changed in D8, branch A. New file size 2",
            "9",
        ],
    );
    commit(&["commit", "-q", "-am", "D8"]);

    let state = assert_integrate_matches_git_merge(root, "A", "B");
    assert!(
        matches!(state, MergeState::Merged { .. }),
        "t6401 merges cleanly"
    );
    assert_integrate_matches_git_merge(root, "B", "A");
}

/// Upstream t6404: three merge bases (nested virtual merges) and a conflicted final merge,
/// with every commit sharing one committer date, then a binary file on top.
#[test]
fn t6404_recursive_merge_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_identity_repo();
    let root = history.worktree();
    let committer = "2006-12-12 23:28:00 +0100";
    let commit_at = |second: u32, args: &[&str]| {
        git_dated(
            root,
            &format!("2006-12-12 23:00:0{second}"),
            committer,
            args,
        );
    };
    let record_merge = |second: u32, other: &str, content: &str, msg: &str| {
        let other_oid = git(root, &["rev-parse", other]);
        fs::write(root.join(".git/MERGE_HEAD"), other_oid).unwrap();
        fs::write(root.join("a1"), content).unwrap();
        hist_git(root, &["update-index", "a1"]);
        commit_at(second, &["commit", "-q", "-m", msg]);
    };
    fs::write(root.join("a1"), "1\n").unwrap();
    hist_git(root, &["add", "a1"]);
    commit_at(0, &["commit", "-q", "-m", "1", "a1"]);
    hist_git(root, &["switch", "-q", "-c", "A", "main"]);
    fs::write(root.join("a1"), "A\n").unwrap();
    commit_at(1, &["commit", "-q", "-m", "A", "a1"]);
    hist_git(root, &["switch", "-q", "-c", "B", "main"]);
    fs::write(root.join("a1"), "B\n").unwrap();
    commit_at(2, &["commit", "-q", "-m", "B", "a1"]);
    hist_git(root, &["switch", "-q", "-c", "D", "A"]);
    record_merge(3, "B", "D\n", "D");
    hist_git(root, &["symbolic-ref", "HEAD", "refs/heads/other"]);
    fs::write(root.join("a1"), "2\n").unwrap();
    commit_at(4, &["commit", "-q", "-m", "2", "a1"]);
    hist_git(root, &["switch", "-q", "-c", "C"]);
    fs::write(root.join("a1"), "C\n").unwrap();
    commit_at(5, &["commit", "-q", "-m", "C", "a1"]);
    hist_git(root, &["switch", "-q", "-c", "E", "C"]);
    record_merge(6, "B", "E\n", "E");
    hist_git(root, &["switch", "-q", "-c", "G", "E"]);
    record_merge(7, "A", "G\n", "G");
    hist_git(root, &["switch", "-q", "-c", "F", "D"]);
    record_merge(8, "C", "F\n", "F");

    // All three bases share a committer date; their order decides the virtual base.
    let rev = |name: &str| -> ObjectId { git(root, &["rev-parse", name]).trim().parse().unwrap() };
    let git_bases: Vec<ObjectId> = git(root, &["merge-base", "--all", "F", "G"])
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    let grit_bases = grit_lib::revwalk::merge_base::merge_bases_all(
        &open_grit_repo(root),
        &[rev("F"), rev("G")],
    )
    .unwrap();
    assert_eq!(
        grit_bases, git_bases,
        "tied merge bases must keep git's order"
    );
    assert_eq!(grit_bases.len(), 3);

    let state = assert_integrate_matches_git_merge(root, "F", "G");
    assert!(
        matches!(state, MergeState::Conflicted { .. }),
        "t6404 final merge conflicts"
    );
    assert_integrate_matches_git_merge(root, "G", "F");

    hist_git(root, &["switch", "-q", "F"]);
    fs::write(root.join("binary-file"), b"\0").unwrap();
    hist_git(root, &["add", "binary-file"]);
    hist_git(root, &["commit", "-q", "-m", "binary"]);
    hist_git(root, &["switch", "-q", "G"]);
    fs::write(root.join("binary-file"), b"\0\0").unwrap();
    hist_git(root, &["add", "binary-file"]);
    hist_git(root, &["commit", "-q", "-m", "binary2"]);
    assert_integrate_matches_git_merge(root, "G", "F");
}

/// Upstream t6431: rename/delete conflicts resolved differently on each side of a
/// criss-cross, then merged.
#[test]
fn t6431_criss_cross_rename_delete_matches_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = init_identity_repo();
    let root = history.worktree();
    let mut step = 0u32;
    let mut run = |args: &[&str]| {
        step += 1;
        let date = format!("{} +0000", 1_300_000_000 + step * 60);
        git_dated(root, &date, &date, args);
    };
    for n in 1..=10 {
        write_lines(root, &format!("data/{n}"), &[&n.to_string()]);
    }
    hist_git(root, &["add", "data"]);
    run(&["commit", "-q", "-m", "A"]);
    hist_git(root, &["branch", "A"]);
    hist_git(root, &["switch", "-q", "-c", "B", "A"]);
    hist_git(root, &["rm", "-q", "data/9"]);
    run(&["commit", "-q", "-m", "B"]);
    hist_git(root, &["branch", "D"]);
    hist_git(root, &["switch", "-q", "D"]);
    write_lines(root, "data/testD", &["testD"]);
    hist_git(root, &["add", "data"]);
    run(&["commit", "-q", "-m", "D"]);
    hist_git(root, &["switch", "-q", "-c", "C", "A"]);
    hist_git(root, &["mv", "data/9", "data/new-9"]);
    run(&["commit", "-q", "-m", "C"]);
    hist_git(root, &["branch", "E"]);
    hist_git(root, &["switch", "-q", "E"]);
    write_lines(root, "data/testE", &["testE"]);
    hist_git(root, &["add", "data"]);
    run(&["commit", "-q", "-m", "E"]);
    hist_git(root, &["switch", "-q", "B"]);
    let conflicted = grit_test_support::git_cmd(&["merge", "-q", "E"])
        .in_dir(root)
        .exec();
    assert!(!conflicted.ok(), "merging E into B must conflict");
    hist_git(root, &["add", "data"]);
    run(&["commit", "-q", "-m", "F"]);
    hist_git(root, &["branch", "F"]);
    hist_git(root, &["switch", "-q", "C"]);
    let conflicted = grit_test_support::git_cmd(&["merge", "-q", "D"])
        .in_dir(root)
        .exec();
    assert!(!conflicted.ok(), "merging D into C must conflict");
    hist_git(root, &["add", "data"]);
    run(&["commit", "-q", "-m", "G"]);
    hist_git(root, &["branch", "G"]);

    assert_integrate_matches_git_merge(root, "C", "F");
    assert_integrate_matches_git_merge(root, "F", "C");
}

/// Build a criss-cross: `a1` and `b1` fork from `base`, branch `ours` merges `b1` into `a1`
/// and branch `theirs` merges `a1` into `b1`. Each step edits the work tree through its
/// closure; merges that stop on conflicts are resolved by the matching closure.
fn criss_cross_with(
    base: impl Fn(&std::path::Path),
    a1: impl Fn(&std::path::Path),
    b1: impl Fn(&std::path::Path),
    resolve_ours: impl Fn(&std::path::Path),
    resolve_theirs: impl Fn(&std::path::Path),
) -> HistoryRepo {
    let history = init_identity_repo();
    let root = history.worktree();
    let at = |step: u32, args: &[&str]| {
        let date = format!("{} +0000", 1_400_000_000 + step * 60);
        git_dated(root, &date, &date, args);
    };
    let snapshot = |step: u32, msg: &str| {
        hist_git(root, &["add", "-A"]);
        at(step, &["commit", "-q", "-m", msg]);
    };
    let merge = |step: u32, other: &str, resolve: &dyn Fn(&std::path::Path)| {
        let _ = grit_test_support::git_cmd(&["merge", "-q", "--no-commit", other])
            .in_dir(root)
            .exec();
        resolve(root);
        hist_git(root, &["add", "-A"]);
        at(
            step,
            &["commit", "-q", "--no-edit", "-m", &format!("merge {other}")],
        );
    };
    base(root);
    snapshot(1, "base");
    hist_git(root, &["switch", "-q", "-c", "a1"]);
    a1(root);
    snapshot(2, "a1");
    hist_git(root, &["switch", "-q", "-c", "b1", "main"]);
    b1(root);
    snapshot(3, "b1");
    hist_git(root, &["switch", "-q", "-c", "ours", "a1"]);
    merge(4, "b1", &resolve_ours);
    hist_git(root, &["switch", "-q", "-c", "theirs", "b1"]);
    merge(5, "a1", &resolve_theirs);
    let bases = git(root, &["merge-base", "--all", "ours", "theirs"]);
    assert_eq!(
        bases.lines().count(),
        2,
        "fixture must have two merge bases"
    );
    history
}

fn write(path: &'static str, content: &'static [u8]) -> impl Fn(&std::path::Path) {
    move |root| fs::write(root.join(path), content).unwrap()
}

fn remove(path: &'static str) -> impl Fn(&std::path::Path) {
    move |root| {
        let _ = fs::remove_file(root.join(path));
    }
}

fn link(path: &'static str, target: &'static str) -> impl Fn(&std::path::Path) {
    move |root| {
        let _ = fs::remove_file(root.join(path));
        symlink(target, root.join(path)).unwrap();
    }
}

/// Merge each criss-cross side into the other with grit and git; both must agree.
fn assert_criss_cross_sides_match_git(history: &HistoryRepo) {
    assert_integrate_matches_git_merge(history.worktree(), "ours", "theirs");
    assert_integrate_matches_git_merge(history.worktree(), "theirs", "ours");
}

#[test]
fn criss_cross_binary_bases_match_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = criss_cross_with(
        write("f.bin", b"\0base"),
        write("f.bin", b"\0a1"),
        write("f.bin", b"\0b1"),
        write("f.bin", b"\0a1"),
        write("f.bin", b"\0b1"),
    );
    assert_criss_cross_sides_match_git(&history);
}

#[test]
fn criss_cross_modify_delete_bases_match_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = criss_cross_with(
        write("f", b"base\n"),
        remove("f"),
        write("f", b"modified\n"),
        remove("f"),
        write("f", b"modified\n"),
    );
    assert_criss_cross_sides_match_git(&history);
}

#[test]
fn criss_cross_symlink_bases_match_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = criss_cross_with(
        link("l", "target-base"),
        link("l", "target-a1"),
        link("l", "target-b1"),
        link("l", "target-a1"),
        link("l", "target-b1"),
    );
    assert_criss_cross_sides_match_git(&history);
}

#[test]
fn criss_cross_add_add_bases_match_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let history = criss_cross_with(
        write("keep", b"keep\n"),
        write("new", b"from a1\n"),
        write("new", b"from b1\n"),
        write("new", b"from a1\n"),
        write("new", b"from b1\n"),
    );
    assert_criss_cross_sides_match_git(&history);
}

#[test]
fn criss_cross_diff3_inner_markers_match_git() {
    if !skip_merge_tree_git() {
        return;
    }
    let cc = criss_cross_conflicting_resolutions();
    hist_git(cc.dir.path(), &["config", "merge.conflictStyle", "diff3"]);
    assert_integrate_matches_git_merge(cc.dir.path(), "ours", "theirs");
    assert_integrate_matches_git_merge(cc.dir.path(), "theirs", "ours");
}
