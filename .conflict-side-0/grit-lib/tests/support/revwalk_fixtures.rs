//! Independently authored histories for rev-list simplification tests (git as oracle).

use std::path::Path;

use grit_lib::objects::ObjectId;

use super::history_harness::{commit_file, git, HistoryRepo};

/// Path-limited merge-back subgraph from the current `main_branch` tip (for multi-tip oracles).
fn append_merge_back_simplify_history(wt: &Path, prefix: &str, main_branch: &str) -> ObjectId {
    let side = format!("{prefix}-side");
    git(wt, &["branch", &side]);
    git(wt, &["checkout", &side]);
    commit_file(
        wt,
        &format!("{prefix}-side-only.txt"),
        "side-root\n",
        &format!("{prefix} side root"),
    );

    git(wt, &["checkout", main_branch]);
    commit_file(
        wt,
        "content.txt",
        &format!("{prefix}-main-v2\n"),
        &format!("{prefix} main touch path"),
    );
    commit_file(
        wt,
        &format!("{prefix}-aux.txt"),
        "aux-2\n",
        &format!("{prefix} main aux"),
    );

    git(wt, &["checkout", &side]);
    commit_file(
        wt,
        "content.txt",
        &format!("{prefix}-main-v2\n"),
        &format!("{prefix} side same path"),
    );
    commit_file(
        wt,
        &format!("{prefix}-aux.txt"),
        "aux-2\n",
        &format!("{prefix} side align aux"),
    );
    commit_file(
        wt,
        &format!("{prefix}-extra.txt"),
        "extra\n",
        &format!("{prefix} side extra"),
    );
    git(
        wt,
        &[
            "merge",
            main_branch,
            "-m",
            &format!("{prefix} merge main into side"),
        ],
    );
    commit_file(
        wt,
        &format!("{prefix}-noise-side.txt"),
        "n1\n",
        &format!("{prefix} side noise"),
    );

    git(wt, &["checkout", main_branch]);
    commit_file(
        wt,
        &format!("{prefix}-noise-main.txt"),
        "n2\n",
        &format!("{prefix} main noise"),
    );
    git(
        wt,
        &["merge", "--no-edit", &side, "-m", &format!("{prefix} back")],
    );
    commit_file(
        wt,
        "content.txt",
        &format!("{prefix}-main-v3\n"),
        &format!("{prefix} main final path"),
    );

    git(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("tip oid")
}

/// Two independent merge-back tips (`a0-tip`, `b0-tip`) sharing one root commit.
pub fn setup_dual_merge_back_simplify_oracle(wt: &Path) -> (String, String) {
    commit_file(wt, "content.txt", "oracle-root\n", "oracle shared root");
    git(wt, &["tag", "oracle-root"]);
    append_merge_back_simplify_history(wt, "a0", "main");
    git(wt, &["tag", "a0-tip"]);
    git(wt, &["checkout", "oracle-root"]);
    git(wt, &["checkout", "-b", "b0-main"]);
    append_merge_back_simplify_history(wt, "b0", "b0-main");
    git(wt, &["tag", "b0-tip"]);
    ("a0-tip".to_string(), "b0-tip".to_string())
}

/// Merge-heavy history with path-limited rev-list modes (t6012-style semantics).
pub fn setup_path_simplify_history(wt: &Path) -> ObjectId {
    commit_file(wt, "content.txt", "seed\n", "rw6012 root a");
    commit_file(wt, "aux.txt", "aux-1\n", "rw6012 aux on main");
    git(wt, &["tag", "rw6012-a"]);

    git(wt, &["branch", "rw6012-side"]);
    git(wt, &["checkout", "rw6012-side"]);
    commit_file(
        wt,
        "side-only.txt",
        "side-root\n",
        "rw6012 unrelated side root",
    );
    git(wt, &["tag", "rw6012-j"]);
    git(wt, &["checkout", "main"]);

    commit_file(wt, "content.txt", "main-v2\n", "rw6012 main touch path");
    commit_file(wt, "aux.txt", "aux-2\n", "rw6012 main aux");
    git(wt, &["tag", "rw6012-b"]);

    git(wt, &["checkout", "rw6012-side"]);
    commit_file(
        wt,
        "content.txt",
        "main-v2\n",
        "rw6012 side same path content",
    );
    commit_file(wt, "aux.txt", "aux-2\n", "rw6012 side align aux");
    git(wt, &["tag", "rw6012-c"]);
    commit_file(wt, "extra.txt", "extra\n", "rw6012 side extra path");
    git(wt, &["tag", "rw6012-d"]);
    git(wt, &["merge", "main", "-m", "rw6012 merge main into side"]);
    git(wt, &["tag", "rw6012-e"]);

    commit_file(wt, "noise-side.txt", "n1\n", "rw6012 side noise");
    git(wt, &["tag", "rw6012-f"]);

    git(wt, &["checkout", "main"]);
    commit_file(wt, "noise-main.txt", "n2\n", "rw6012 main noise");
    git(wt, &["tag", "rw6012-g"]);
    git(
        wt,
        &[
            "merge",
            "--no-edit",
            "rw6012-side",
            "-m",
            "rw6012 merge side into main",
        ],
    );
    git(wt, &["tag", "rw6012-h"]);

    commit_file(
        wt,
        "content.txt",
        "main-v3\n",
        "rw6012 main final path change",
    );
    git(wt, &["tag", "rw6012-i"]);

    git(
        wt,
        &[
            "merge",
            "--allow-unrelated-histories",
            "rw6012-j",
            "-m",
            "rw6012 merge unrelated",
        ],
    );
    git(wt, &["tag", "rw6012-k"]);

    commit_file(wt, "noise-main.txt", "n3\n", "rw6012 immaterial noise");
    git(wt, &["tag", "rw6012-l"]);

    git(wt, &["rev-parse", "rw6012-l"])
        .trim()
        .parse()
        .expect("tip oid")
}

/// Treesame / merge-heavy graph for path-limited walks (t6111-style semantics).
pub fn setup_treesame_history(wt: &Path) -> (ObjectId, ObjectId) {
    commit_file(wt, "content.txt", "v0\n", "rw6111 start");
    git(wt, &["tag", "rw6111-a"]);

    git(wt, &["branch", "rw6111-other"]);
    commit_file(wt, "content.txt", "v1\n", "rw6111 main line");
    git(wt, &["tag", "rw6111-b"]);

    git(wt, &["checkout", "rw6111-other"]);
    commit_file(wt, "peer.txt", "peer\n", "rw6111 other file");
    git(wt, &["tag", "rw6111-c"]);
    git(wt, &["checkout", "main"]);
    git(wt, &["merge", "--no-edit", "rw6111-other"]);
    git(wt, &["tag", "rw6111-d"]);

    git(wt, &["checkout", "main"]);
    commit_file(
        wt,
        "content.txt",
        "v2\n",
        "rw6111 main line before side work",
    );
    git(wt, &["tag", "rw6111-f"]);

    git(wt, &["branch", "rw6111-fiddle"]);
    git(wt, &["checkout", "-b", "rw6111-part2"]);
    commit_file(wt, "content.txt", "part2\n", "rw6111 part two");
    git(wt, &["tag", "rw6111-h"]);
    git(wt, &["checkout", "rw6111-fiddle"]);
    commit_file(wt, "content.txt", "bad\n", "rw6111 fiddle bad");
    git(wt, &["revert", "--no-edit", "HEAD"]);
    git(wt, &["tag", "rw6111-j"]);
    git(wt, &["checkout", "main"]);
    git(
        wt,
        &[
            "merge",
            "--no-ff",
            "rw6111-fiddle",
            "-m",
            "rw6111 merge fiddle",
        ],
    );
    git(wt, &["tag", "rw6111-k"]);
    commit_file(wt, "content.txt", "part1\n", "rw6111 part one");
    git(wt, &["tag", "rw6111-l"]);
    git(wt, &["merge", "-X", "theirs", "--no-edit", "rw6111-part2"]);
    git(wt, &["tag", "rw6111-m"]);

    let f: ObjectId = git(wt, &["rev-parse", "rw6111-f"])
        .trim()
        .parse()
        .expect("f");
    let m: ObjectId = git(wt, &["rev-parse", "rw6111-m"])
        .trim()
        .parse()
        .expect("m");
    (f, m)
}

/// Fork early on trunk, advance trunk and fork separately, merge trunk into fork.
///
/// ```text
///         Tmid ─ Tpre ─ Ttip   (main)
///        /
/// Troot ─┤
///        \\ Sone ─ Stwo ─ M     (fpb-fork tip; M merges main at Ttip)
/// ```
///
/// Used to compare `rev-list --first-parent --boundary` against system Git.
pub fn setup_first_parent_boundary_history(wt: &Path) -> ObjectId {
    commit_file(wt, "trunk/root.txt", "root\n", "fpb trunk root");
    git(wt, &["branch", "fpb-fork"]);
    git(wt, &["checkout", "fpb-fork"]);
    commit_file(wt, "fork/one.txt", "1\n", "fpb fork first");
    commit_file(wt, "fork/two.txt", "2\n", "fpb fork second");
    git(wt, &["checkout", "main"]);
    commit_file(wt, "trunk/mid.txt", "m\n", "fpb trunk mid");
    commit_file(wt, "trunk/pre-tip.txt", "p\n", "fpb trunk pre-tip");
    commit_file(wt, "trunk/tip.txt", "t\n", "fpb trunk tip");
    git(wt, &["checkout", "fpb-fork"]);
    git(
        wt,
        &[
            "merge",
            "--no-edit",
            "main",
            "-m",
            "fpb merge trunk into fork",
        ],
    );
    git(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("tip oid")
}

#[allow(dead_code)]
pub fn fresh_repo() -> HistoryRepo {
    HistoryRepo::init()
}
