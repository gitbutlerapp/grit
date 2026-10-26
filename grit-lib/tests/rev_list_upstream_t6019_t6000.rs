//! Focused ports of upstream `t/t6019-rev-list-ancestry-path.sh` and `t/t6000-rev-list-misc.sh`
//! (`--unpacked` object walk) for grit vs system `git` parity.

use crate::support;

use std::collections::HashSet;

use grit_lib::objects::ObjectId;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};
use support::{
    assert_rev_list_commits_match_git, build_commit_dag, commit_file, expand_token_for_rev_list,
    git, git_rev_list, git_rev_list_object_set, grit_rev_list_object_set, open_grit_repo,
    HistoryRepo,
};

fn tag_commits(
    worktree: &std::path::Path,
    labels: &[&str],
    oids: &std::collections::HashMap<String, ObjectId>,
) {
    for name in labels {
        let oid = oids
            .get(*name)
            .unwrap_or_else(|| panic!("missing label {name}"));
        git(
            worktree,
            &["update-ref", &format!("refs/tags/{name}"), &oid.to_hex()],
        );
    }
}

/// DAG from upstream `t6019-rev-list-ancestry-path.sh` setup (labels A..M).
fn setup_t6019_main_dag(worktree: &std::path::Path) -> std::collections::HashMap<String, ObjectId> {
    let oids = build_commit_dag(
        worktree,
        &[
            ("A", "base"),
            ("B", "A"),
            ("C", "B"),
            ("D", "C"),
            ("E", "D"),
            ("F", "E"),
            ("G", "C"),
            ("H", "G"),
            ("H", "E"),
            ("I", "H"),
            ("J", "I"),
            ("J", "F"),
            ("K", "A"),
            ("L", "K"),
            ("L", "J"),
            ("M", "L"),
        ],
        &[("M", "refs/heads/main")],
    );
    tag_commits(
        worktree,
        &[
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M",
        ],
        &oids,
    );
    git(worktree, &["tag", "-f", "M.t", &oids["M"].to_hex()]);
    git(worktree, &["tag", "-f", "G.t", &oids["G"].to_hex()]);
    oids
}

#[test]
fn t6019_ancestry_path_upstream_cases_match_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = setup_t6019_main_dag(wt);
    let grit = open_grit_repo(wt);
    let d = oids["D"].to_hex();
    let m = oids["M"].to_hex();
    let range = format!("{d}..{m}");

    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &[&range],
        &[m.clone()],
        &[d.clone()],
        RevListOptions::default(),
        "t6019 D..M",
    );
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--ancestry-path", &range],
        &[m.clone()],
        &[d.clone()],
        RevListOptions {
            ancestry_path: true,
            ..RevListOptions::default()
        },
        "t6019 --ancestry-path D..M",
    );

    let m_tag = "M.t";
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &[&range, "--", m_tag],
        &[m.clone()],
        &[d.clone(), m_tag.to_owned()],
        RevListOptions::default(),
        "t6019 D..M -- M.t",
    );
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--ancestry-path", &range, "--", m_tag],
        &[m.clone()],
        &[d.clone(), m_tag.to_owned()],
        RevListOptions {
            ancestry_path: true,
            ..RevListOptions::default()
        },
        "t6019 --ancestry-path D..M -- M.t",
    );

    let f = oids["F"].to_hex();
    let i = oids["I"].to_hex();
    let sym = format!("{f}...{i}");
    let mut sym_opts = RevListOptions::default();
    let (sym_pos, sym_neg) = expand_token_for_rev_list(&grit, &sym, &mut sym_opts).expect("F...I");
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &[&sym],
        &sym_pos,
        &sym_neg,
        sym_opts.clone(),
        "t6019 F...I",
    );
    assert_rev_list_commits_match_git(
        wt,
        &grit,
        &["--ancestry-path", &sym],
        &sym_pos,
        &sym_neg,
        RevListOptions {
            ancestry_path: true,
            ..sym_opts
        },
        "t6019 --ancestry-path F...I",
    );
}

#[test]
fn t6019_criss_cross_ancestry_path_empty_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[
            ("A", "base"),
            ("B", "A"),
            ("C", "A"),
            ("X", "B"),
            ("X", "C"),
            ("Y", "C"),
            ("Y", "B"),
        ],
        &[
            ("X", "refs/heads/xbc"),
            ("Y", "refs/heads/xcb"),
            ("A", "refs/heads/main"),
        ],
    );
    git(wt, &["tag", "-f", "xbc", &oids["X"].to_hex()]);
    git(wt, &["tag", "-f", "xcb", &oids["Y"].to_hex()]);
    let grit = open_grit_repo(wt);

    let git_empty = git_rev_list(wt, &["--ancestry-path", "xcb..xbc"]);
    assert!(
        git_empty.is_empty(),
        "git expected empty ancestry-path xcb..xbc"
    );
    let xcb = oids["Y"].to_hex();
    let xbc = oids["X"].to_hex();
    let grit_empty = rev_list(
        &grit,
        &[xbc.clone()],
        &[xcb.clone()],
        &RevListOptions {
            ancestry_path: true,
            ..RevListOptions::default()
        },
    )
    .expect("grit xcb..xbc")
    .commits;
    assert_eq!(grit_empty, git_empty);
}

#[test]
fn t6000_rev_list_unpacked_objects_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "seed.txt", "seed\n", "seed");
    git(wt, &["repack", "-ad"]);
    let unpacked_tip = commit_file(wt, "after.txt", "after repack\n", "unpacked");
    git(wt, &["tag", "-f", "unpacked", &unpacked_tip.to_hex()]);

    let git_expect = git_rev_list_object_set(
        wt,
        &["unpacked~1..unpacked", "--objects", "--no-object-names"],
    );
    let git_all_unpacked = git_rev_list_object_set(
        wt,
        &["--all", "--objects", "--unpacked", "--no-object-names"],
    );
    assert_eq!(
        git_all_unpacked, git_expect,
        "git --unpacked set should match unpacked^.. window"
    );

    let grit_expect = grit_rev_list_object_set(
        wt,
        &[String::from("unpacked")],
        &[String::from("unpacked~1")],
        None,
    );
    assert_eq!(grit_expect, git_expect, "grit unpacked~1..unpacked objects");

    let grit_repo = open_grit_repo(wt);
    let result = rev_list(
        &grit_repo,
        &[],
        &[],
        &RevListOptions {
            all_refs: true,
            objects: true,
            no_object_names: true,
            unpacked_only: true,
            quiet: true,
            ..RevListOptions::default()
        },
    )
    .expect("grit --all --unpacked --objects");
    let mut grit_set: HashSet<ObjectId> = HashSet::new();
    grit_set.extend(result.commits);
    grit_set.extend(result.objects.iter().map(|(o, _)| *o));
    assert_eq!(
        grit_set, git_all_unpacked,
        "grit --all --unpacked --objects"
    );
}
