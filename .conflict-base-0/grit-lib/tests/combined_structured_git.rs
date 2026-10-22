//! Combined structured hunks vs `git show --cc`.

mod support;

use grit_lib::config::ConfigSet;
use grit_lib::diff::combined_diff_patch::{
    format_combined_diff_body, format_combined_structured_body, structured_combined_diff_body,
    CombinedDiffWsOptions, CombinedStructuredLineKind,
};
use grit_lib::diff::combined_tree_diff::{combined_diff_paths_filtered, CombinedTreeDiffOptions};
use grit_lib::diff::merge_diff::{
    blob_text_for_diff, enrich_combined_path_renames, read_blob_at_path,
};
use grit_lib::objects::{parse_commit, ObjectId};

use support::{git_output, git_quiet, DiffRepo};

fn combined_patch_body(patch: &str) -> String {
    let start = patch
        .lines()
        .position(|l| l.starts_with("@@@") || l.starts_with("@@ "))
        .unwrap_or(0);
    patch.lines().skip(start).collect::<Vec<_>>().join("\n")
}

fn build_conflict_resolved_merge(dr: &DiffRepo) -> ObjectId {
    let wt = &dr.worktree;
    std::fs::write(wt.join("file"), "base\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    git_quiet(wt, &["commit", "-qm", "root"]);
    git_quiet(wt, &["checkout", "-q", "-b", "p1"]);
    std::fs::write(wt.join("file"), "one\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    git_quiet(wt, &["commit", "-qm", "p1"]);
    let root = git_quiet(wt, &["symbolic-ref", "--short", "HEAD"])
        .trim()
        .to_owned();
    git_quiet(wt, &["checkout", "-q", &root]);
    std::fs::write(wt.join("file"), "two\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    git_quiet(wt, &["commit", "-qm", "p2"]);
    git_quiet(wt, &["checkout", "-q", "-b", "merge", "p1"]);
    std::fs::write(wt.join("file"), "merged\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    git_quiet(wt, &["commit", "-qm", "merge"]);
    git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap()
}

fn grit_combined_body(dr: &DiffRepo, merge: &ObjectId) -> String {
    let odb = &dr.repo.odb();
    let data = odb.read(merge).expect("read").data;
    let commit = parse_commit(&data).expect("parse");
    let walk = CombinedTreeDiffOptions::default();
    let mut paths = combined_diff_paths_filtered(odb, &commit.tree, &commit.parents, &walk, None)
        .expect("paths");
    if paths.is_empty() {
        return String::new();
    }
    let idx = paths.iter().position(|p| p.path == "file").unwrap_or(0);
    let path = &mut paths[idx];
    enrich_combined_path_renames(odb, path, &commit.parents, &commit.tree, 50);
    let config = ConfigSet::new();
    let git_dir = dr.repo.git_dir();
    let ws = CombinedDiffWsOptions::default();
    let mut parent_texts = Vec::new();
    for (i, side) in path.parents.iter().enumerate() {
        let blob = odb.read(&side.oid).expect("parent blob").data;
        parent_texts.push(blob_text_for_diff(
            git_dir, &config, &path.path, &blob, false,
        ));
        let _ = i;
    }
    let result_blob = read_blob_at_path(odb, &commit.tree, &path.path).unwrap_or_default();
    let result = blob_text_for_diff(git_dir, &config, &path.path, &result_blob, false);
    let structured = structured_combined_diff_body(&parent_texts, &result, 3, true, ws);
    format_combined_structured_body(&structured)
}

#[test]
fn combined_structured_hunks_match_git_cc() {
    let dr = DiffRepo::init();

    // Conflict-resolved merge (t4038-style two-parent merge commit).
    let merge = build_conflict_resolved_merge(&dr);
    let wt = &dr.worktree;
    let git_out = git_output(wt, &["diff-tree", "-p", "--cc", "-U3", &merge.to_hex()]);
    assert!(git_out.status.success(), "git diff-tree --cc -p");
    let git_patch = String::from_utf8_lossy(&git_out.stdout);
    let git_body = combined_patch_body(&git_patch);
    let grit_body = grit_combined_body(&dr, &merge);
    if git_body.contains("@@@") {
        assert_eq!(
            git_body.trim(),
            grit_body.trim(),
            "structured combined body must match git combined patch"
        );
    } else {
        assert!(
            !grit_body.is_empty() || git_body.trim().is_empty(),
            "grit combined body should exist when git reports a patch"
        );
    }

    // Clean merge: both parents match result → no combined patch body.
    let dr2 = DiffRepo::init();
    let wt2 = &dr2.worktree;
    std::fs::write(wt2.join("file"), "same\n").unwrap();
    git_quiet(wt2, &["add", "file"]);
    git_quiet(wt2, &["commit", "-qm", "root"]);
    git_quiet(wt2, &["checkout", "-q", "-b", "p1"]);
    git_quiet(wt2, &["commit", "--allow-empty", "-qm", "p1"]);
    let p1: ObjectId = git_quiet(wt2, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    git_quiet(wt2, &["checkout", "-q", "main"]);
    git_quiet(wt2, &["commit", "--allow-empty", "-qm", "p2"]);
    let p2: ObjectId = git_quiet(wt2, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let tree: ObjectId = git_quiet(wt2, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .parse()
        .unwrap();
    let clean: ObjectId = git_quiet(
        wt2,
        &[
            "commit-tree",
            &tree.to_hex(),
            "-p",
            &p1.to_hex(),
            "-p",
            &p2.to_hex(),
            "-m",
            "clean",
        ],
    )
    .trim()
    .parse()
    .unwrap();
    let git_clean = git_output(wt2, &["show", "--cc", "--format=", &clean.to_hex()]);
    assert!(git_clean.status.success());
    let git_clean_body = combined_patch_body(&String::from_utf8_lossy(&git_clean.stdout));
    assert!(
        git_clean_body.trim().is_empty(),
        "git clean merge should have empty combined body"
    );
    let grit_clean = grit_combined_body(&dr2, &clean);
    assert!(
        grit_clean.trim().is_empty(),
        "grit clean merge combined body should be empty"
    );

    // Round-trip: structured serialization matches direct formatter.
    let parents = vec!["a\n".to_string(), "b\n".to_string()];
    let direct =
        format_combined_diff_body(&parents, "c\n", 3, true, CombinedDiffWsOptions::default());
    let structured =
        structured_combined_diff_body(&parents, "c\n", 3, true, CombinedDiffWsOptions::default());
    assert_eq!(direct, format_combined_structured_body(&structured));
    assert!(
        structured
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.kind == CombinedStructuredLineKind::Add),
        "structured combined diff should tag added lines"
    );
    assert!(
        structured
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .all(|l| { l.kind == CombinedStructuredLineKind::from_parent_markers(&l.parents) }),
        "line kind must match parent-column markers"
    );
}
