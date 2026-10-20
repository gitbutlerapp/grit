//! Seeded three-way tree merge matrix vs `git merge-tree --write-tree`.

mod support;

use std::fs;

use grit_lib::objects::ObjectId;

use support::git as hist_git;
use support::{
    assert_merge_commits_match_git, commit_file, open_grit_repo, skip_merge_tree_git,
    CommitMergeLabels, HistoryRepo,
};

const CASE_COUNT: u32 = 220;

/// Deterministic mixing for path and content bytes from the case id.
fn case_mix(case: u32, salt: u32) -> u32 {
    case.wrapping_mul(1_609_481_113)
        .wrapping_add(salt.wrapping_mul(3_745_313))
}

#[test]
fn merge_trees_three_way_seeded_git_compat_matrix() {
    if !skip_merge_tree_git() {
        return;
    }
    for case in 0..CASE_COUNT {
        let history = init_repo();
        let (base, ours, theirs) = build_case_triple(&history, case);
        let grit = open_grit_repo(history.worktree());
        let labels = CommitMergeLabels::from_commits(&ours, &theirs);
        assert_merge_commits_match_git(
            &history,
            &grit,
            &base,
            &ours,
            &theirs,
            labels.presentation(),
            &format!("seeded case {case}"),
        );
    }
}

fn init_repo() -> HistoryRepo {
    let repo = HistoryRepo::init();
    hist_git(repo.worktree(), &["config", "core.autocrlf", "false"]);
    repo
}

fn build_case_triple(history: &HistoryRepo, case: u32) -> (ObjectId, ObjectId, ObjectId) {
    let wt = history.worktree();
    let slot = case_mix(case, 0) % 10_000;
    let kind = case % 11;
    let base = match kind {
        0 => scenario_both_modify(wt, slot),
        1 => scenario_add_add(wt, slot),
        2 => scenario_modify_delete(wt, slot, true),
        3 => scenario_modify_delete(wt, slot, false),
        4 => scenario_rename_edit(wt, slot),
        5 => scenario_df_ours_file_theirs_dir(wt, slot),
        6 => scenario_theirs_only_nested_dir(wt, slot),
        7 => scenario_nested_df(wt, slot),
        8 => scenario_mode_change(wt, slot),
        9 => scenario_deep_tree(wt, slot, case),
        _ => scenario_dual_rename(wt, slot),
    };
    let ours = hist_git(wt, &["rev-parse", "ours"])
        .trim()
        .parse()
        .expect("ours");
    let theirs = hist_git(wt, &["rev-parse", "main"])
        .trim()
        .parse()
        .expect("main");
    (base, ours, theirs)
}

fn branch_from_base(wt: &std::path::Path, _base: ObjectId, name: &str) {
    hist_git(wt, &["switch", "-c", name]);
}

fn finish_theirs_on_main(wt: &std::path::Path) {
    hist_git(wt, &["switch", "-f", "main"]);
}

fn scenario_both_modify(wt: &std::path::Path, slot: u32) -> ObjectId {
    let path = format!("both/{slot}.txt");
    let base = commit_file(wt, &path, "base line\n", "base");
    branch_from_base(wt, base, "ours");
    commit_file(wt, &path, &format!("ours {slot}\n"), "ours");
    finish_theirs_on_main(wt);
    commit_file(wt, &path, &format!("theirs {slot}\n"), "theirs");
    base
}

fn scenario_add_add(wt: &std::path::Path, slot: u32) -> ObjectId {
    let path = format!("added/{slot}.txt");
    let base = commit_file(wt, "root.txt", "root\n", "base");
    branch_from_base(wt, base, "ours");
    commit_file(wt, &path, &format!("ours add {slot}\n"), "ours");
    finish_theirs_on_main(wt);
    commit_file(wt, &path, &format!("theirs add {slot}\n"), "theirs");
    base
}

fn scenario_modify_delete(wt: &std::path::Path, slot: u32, ours_mod: bool) -> ObjectId {
    let path = format!("md/{slot}.txt");
    let base = commit_file(wt, &path, "keep\n", "base");
    branch_from_base(wt, base, "ours");
    if ours_mod {
        commit_file(wt, &path, "modified\n", "ours");
    } else {
        hist_git(wt, &["rm", &path]);
        hist_git(wt, &["commit", "-m", "ours del"]);
    }
    finish_theirs_on_main(wt);
    if ours_mod {
        hist_git(wt, &["rm", &path]);
        hist_git(wt, &["commit", "-m", "theirs del"]);
    } else {
        commit_file(wt, &path, "modified\n", "theirs");
    }
    base
}

fn scenario_rename_edit(wt: &std::path::Path, slot: u32) -> ObjectId {
    let old = format!("ren/{slot}/old.txt");
    let new = format!("ren/{slot}/new.txt");
    let base = commit_file(wt, &old, "content\n", "base");
    branch_from_base(wt, base, "ours");
    hist_git(wt, &["mv", &old, &new]);
    commit_file(wt, &new, "edited ours\n", "ours");
    finish_theirs_on_main(wt);
    commit_file(wt, &old, "edited theirs\n", "theirs");
    base
}

fn scenario_df_ours_file_theirs_dir(wt: &std::path::Path, slot: u32) -> ObjectId {
    // Same shape as merge-ort t6400: file at `dir/foo` vs file at `dir`.
    let dir = format!("df/{slot}/dir");
    let nested = format!("{dir}/foo");
    let base = commit_file(wt, "seed.txt", "seed\n", "base");
    branch_from_base(wt, base, "ours");
    commit_file(wt, &nested, "foo\n", "ours nested file");
    finish_theirs_on_main(wt);
    commit_file(wt, &dir, "file dir\n", "theirs file at dir path");
    base
}

fn scenario_theirs_only_nested_dir(wt: &std::path::Path, slot: u32) -> ObjectId {
    let base = commit_file(wt, &format!("nest2/{slot}/a.txt"), "a\n", "base");
    branch_from_base(wt, base, "ours");
    finish_theirs_on_main(wt);
    let nested = format!("nest2/{slot}/newdir/x.txt");
    fs::create_dir_all(wt.join("nest2").join(slot.to_string()).join("newdir")).expect("mkdir");
    commit_file(wt, &nested, "x\n", "theirs nested dir");
    base
}

fn scenario_nested_df(wt: &std::path::Path, slot: u32) -> ObjectId {
    let base = commit_file(wt, &format!("nest/{slot}/a.txt"), "a\n", "base");
    branch_from_base(wt, base, "ours");
    finish_theirs_on_main(wt);
    let nested = format!("nest/{slot}/newdir/x.txt");
    fs::create_dir_all(wt.join("nest").join(slot.to_string()).join("newdir")).expect("mkdir");
    commit_file(wt, &nested, "x\n", "theirs nested dir");
    base
}

fn scenario_mode_change(wt: &std::path::Path, slot: u32) -> ObjectId {
    let path = format!("mode/{slot}.sh");
    let base = commit_file(wt, &path, "#!/bin/sh\n", "base");
    branch_from_base(wt, base, "ours");
    fs::write(wt.join(&path), "#!/bin/sh\n").expect("write");
    hist_git(wt, &["update-index", "--chmod=+x", &path]);
    hist_git(wt, &["commit", "-m", "ours exec"]);
    finish_theirs_on_main(wt);
    commit_file(wt, &path, "#!/bin/sh\necho\n", "theirs");
    base
}

fn scenario_deep_tree(wt: &std::path::Path, slot: u32, case: u32) -> ObjectId {
    let depth = (case_mix(case, 7) % 6) + 3;
    let mut parts = Vec::new();
    for d in 0..depth {
        parts.push(format!("d{d}"));
    }
    let leaf = format!("{}/leaf.txt", parts.join("/"));
    let base = commit_file(wt, &leaf, "deep base\n", "base");
    branch_from_base(wt, base, "ours");
    commit_file(wt, &leaf, &format!("deep ours {slot}\n"), "ours");
    finish_theirs_on_main(wt);
    let sibling = format!("{}/side.txt", parts.join("/"));
    commit_file(wt, &sibling, "side\n", "theirs");
    base
}

fn scenario_dual_rename(wt: &std::path::Path, slot: u32) -> ObjectId {
    let src = format!("dual/{slot}/src.txt");
    let base = commit_file(wt, &src, "body\n", "base");
    branch_from_base(wt, base, "ours");
    let ours_dst = format!("dual/{slot}/ours-dest.txt");
    hist_git(wt, &["mv", &src, &ours_dst]);
    commit_file(wt, &ours_dst, "ours body\n", "ours");
    finish_theirs_on_main(wt);
    let theirs_dst = format!("dual/{slot}/theirs-dest.txt");
    hist_git(wt, &["mv", &src, &theirs_dst]);
    commit_file(wt, &theirs_dst, "theirs body\n", "theirs");
    base
}
