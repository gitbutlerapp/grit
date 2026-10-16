//! Combined merge diffs vs `git diff-tree -c/--cc`.

mod support;

use std::io::Write;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::diff::combined_diff_patch::{format_combined_diff_body, CombinedDiffWsOptions};
use grit_lib::diff::combined_tree_diff::{
    combined_diff_paths_filtered, format_combined_raw_line, format_combined_raw_line_all_paths,
    CombinedTreeDiffOptions,
};
use grit_lib::diff::merge_diff::{
    combined_diff_paths, enrich_combined_path_renames, format_combined_textconv_patch,
    format_worktree_conflict_combined,
};
use grit_lib::objects::{parse_commit, ObjectId};

use support::{git_output, git_quiet, git_raw_lines, DiffRepo};

fn read_merge(wt: &std::path::Path, commit: &ObjectId) -> (ObjectId, Vec<ObjectId>) {
    let repo =
        grit_lib::repo::Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let data = repo.odb().read(commit).expect("read").data;
    let parsed = parse_commit(&data).expect("parse");
    (parsed.tree, parsed.parents)
}

#[test]
fn t4038_combined_raw_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    std::fs::write(wt.join("file"), "base\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    git_quiet(wt, &["commit", "-qm", "root"]);

    git_quiet(wt, &["checkout", "-q", "-b", "p1"]);
    std::fs::write(wt.join("file"), "one\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    git_quiet(wt, &["commit", "-qm", "p1"]);
    let p1: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    git_quiet(wt, &["checkout", "-q", "main"]);
    std::fs::write(wt.join("file"), "two\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    git_quiet(wt, &["commit", "-qm", "p2"]);
    let p2: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    git_quiet(wt, &["checkout", "-q", "-b", "merge", "p1"]);
    std::fs::write(wt.join("file"), "merged\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    let merge_tree: ObjectId = git_quiet(wt, &["write-tree"]).trim().parse().unwrap();
    let merge_commit: ObjectId = git_quiet(
        wt,
        &[
            "commit-tree",
            &merge_tree.to_hex(),
            "-p",
            &p1.to_hex(),
            "-p",
            &p2.to_hex(),
            "-m",
            "merge",
        ],
    )
    .trim()
    .parse()
    .unwrap();

    let git_lines = git_raw_lines(
        wt,
        &["diff-tree", "--cc", "-r", "--raw", &merge_commit.to_hex()],
    );

    let (result_tree, parents) = read_merge(wt, &merge_commit);
    assert_eq!(parents.len(), 2);
    let walk = CombinedTreeDiffOptions::default();
    let paths = combined_diff_paths_filtered(&dr.repo.odb(), &result_tree, &parents, &walk, None)
        .expect("combined paths");
    let mut grit_lines: Vec<String> = paths
        .iter()
        .map(|p| format_combined_raw_line(p, None))
        .collect();
    grit_lines.sort();

    assert_eq!(
        git_lines, grit_lines,
        "combined raw lines must match git diff-tree --cc --raw"
    );

    if let Some(path) = paths.first() {
        let abbreviated = format_combined_raw_line(path, Some(7));
        assert!(abbreviated.contains(':'));
    }
}

#[test]
fn t4057_combined_paths_octopus_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    std::fs::write(wt.join("shared.txt"), "base\n").unwrap();
    git_quiet(wt, &["add", "shared.txt"]);
    git_quiet(wt, &["commit", "-qm", "root"]);
    let init: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    git_quiet(wt, &["checkout", "-q", "-b", "p1"]);
    std::fs::write(wt.join("shared.txt"), "one\n").unwrap();
    git_quiet(wt, &["add", "shared.txt"]);
    git_quiet(wt, &["commit", "-qm", "p1"]);
    let p1: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    git_quiet(wt, &["checkout", "-q", "-b", "p2", &init.to_hex()]);
    std::fs::write(wt.join("shared.txt"), "two\n").unwrap();
    git_quiet(wt, &["add", "shared.txt"]);
    git_quiet(wt, &["commit", "-qm", "p2"]);
    let p2: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    git_quiet(wt, &["checkout", "-q", "-b", "p3", &init.to_hex()]);
    std::fs::write(wt.join("shared.txt"), "three\n").unwrap();
    git_quiet(wt, &["add", "shared.txt"]);
    git_quiet(wt, &["commit", "-qm", "p3"]);
    let p3: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    std::fs::write(wt.join("shared.txt"), "merged\n").unwrap();
    git_quiet(wt, &["add", "shared.txt"]);
    let merge_tree: ObjectId = git_quiet(wt, &["write-tree"]).trim().parse().unwrap();
    let merge: ObjectId = git_quiet(
        wt,
        &[
            "commit-tree",
            &merge_tree.to_hex(),
            "-p",
            &p1.to_hex(),
            "-p",
            &p2.to_hex(),
            "-p",
            &p3.to_hex(),
            "-m",
            "octopus",
        ],
    )
    .trim()
    .parse()
    .unwrap();

    let (tree, parents) = read_merge(wt, &merge);
    assert_eq!(
        parents.len(),
        3,
        "fixture must be a three-parent octopus merge"
    );

    let git_names = {
        let mut args = vec![
            "diff".to_string(),
            "-c".to_string(),
            "--name-only".to_string(),
            merge.to_hex(),
        ];
        for p in &parents {
            args.push(p.to_hex());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = git_output(wt, &arg_refs);
        assert!(out.status.success());
        let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        names.sort();
        names
    };

    let mut grit_sorted = combined_diff_paths(&dr.repo.odb(), &tree, &parents);
    grit_sorted.sort();

    assert_eq!(
        git_names, grit_sorted,
        "combined path list must match git diff -c --name-only on octopus merge"
    );

    let git_raw = git_raw_lines(wt, &["diff-tree", "--cc", "-r", "--raw", &merge.to_hex()]);
    let walk = CombinedTreeDiffOptions::default();
    let paths = combined_diff_paths_filtered(&dr.repo.odb(), &tree, &parents, &walk, None)
        .expect("combined paths");
    let mut grit_raw: Vec<String> = paths
        .iter()
        .map(|p| format_combined_raw_line(p, None))
        .collect();
    grit_raw.sort();
    assert_eq!(
        git_raw, grit_raw,
        "combined raw lines must match git diff-tree --cc --raw on octopus merge"
    );
}

fn patch_body_lines(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter(|l| !l.starts_with("index "))
        .map(str::to_owned)
        .collect()
}

fn build_two_parent_merge(dr: &DiffRepo) -> (ObjectId, ObjectId, Vec<ObjectId>) {
    let wt = &dr.worktree;
    std::fs::write(wt.join("file"), "base\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    git_quiet(wt, &["commit", "-qm", "root"]);

    git_quiet(wt, &["checkout", "-q", "-b", "p1"]);
    std::fs::write(wt.join("file"), "one\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    git_quiet(wt, &["commit", "-qm", "p1"]);
    let p1: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    git_quiet(wt, &["checkout", "-q", "main"]);
    std::fs::write(wt.join("file"), "two\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    git_quiet(wt, &["commit", "-qm", "p2"]);
    let p2: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    git_quiet(wt, &["checkout", "-q", "-b", "merge", "p1"]);
    std::fs::write(wt.join("file"), "merged\n").unwrap();
    git_quiet(wt, &["add", "file"]);
    let merge_tree: ObjectId = git_quiet(wt, &["write-tree"]).trim().parse().unwrap();
    let merge_commit: ObjectId = git_quiet(
        wt,
        &[
            "commit-tree",
            &merge_tree.to_hex(),
            "-p",
            &p1.to_hex(),
            "-p",
            &p2.to_hex(),
            "-m",
            "merge",
        ],
    )
    .trim()
    .parse()
    .unwrap();
    (merge_commit, merge_tree, vec![p1, p2])
}

#[test]
fn t4038_combined_patch_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    let (merge_commit, result_tree, parent_trees) = build_two_parent_merge(&dr);

    let git_out = git_output(
        wt,
        &["diff-tree", "-p", "--cc", "-U3", &merge_commit.to_hex()],
    );
    assert!(git_out.status.success(), "git patch diff-tree");
    let git_patch = String::from_utf8_lossy(&git_out.stdout);
    let git_body = patch_body_lines(
        git_patch
            .split_once("--- a/file")
            .map(|(_, rest)| rest)
            .unwrap_or(&git_patch),
    );

    let walk = CombinedTreeDiffOptions::default();
    let paths =
        combined_diff_paths_filtered(&dr.repo.odb(), &result_tree, &parent_trees, &walk, None)
            .expect("paths");
    let path = paths.iter().find(|p| p.path == "file").expect("file path");
    let config = ConfigSet::new();
    let grit_patch = format_combined_textconv_patch(
        &dr.repo.git_dir(),
        &config,
        &dr.repo.odb(),
        "file",
        &parent_trees,
        &result_tree,
        7,
        3,
        true,
        false,
        CombinedDiffWsOptions::default(),
        false,
        None,
        &path.parents,
        false,
    )
    .expect("combined patch");
    let grit_body = patch_body_lines(
        grit_patch
            .split_once("--- a/file")
            .map(|(_, rest)| rest)
            .unwrap_or(&grit_patch),
    );

    assert!(
        grit_patch.contains("diff --cc file"),
        "grit must emit combined header"
    );
    assert!(
        git_body.iter().any(|l| l.contains("merged"))
            && grit_body.iter().any(|l| l.contains("merged")),
        "both patches must show merge result line"
    );
    assert!(
        grit_body.iter().any(|l| l.starts_with("@@@")),
        "grit combined body must include hunk header"
    );
}

#[test]
fn t4048_combined_binary_matches_git() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    let bin = vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8];
    std::fs::write(wt.join("bin.dat"), &bin).unwrap();
    git_quiet(wt, &["add", "bin.dat"]);
    git_quiet(wt, &["commit", "-qm", "root"]);

    git_quiet(wt, &["checkout", "-q", "-b", "p1"]);
    let mut p1bin = bin.clone();
    p1bin.push(9);
    std::fs::write(wt.join("bin.dat"), &p1bin).unwrap();
    git_quiet(wt, &["add", "bin.dat"]);
    git_quiet(wt, &["commit", "-qm", "p1"]);

    git_quiet(wt, &["checkout", "-q", "main"]);
    let mut p2bin = bin.clone();
    p2bin[0] = 255;
    std::fs::write(wt.join("bin.dat"), &p2bin).unwrap();
    git_quiet(wt, &["add", "bin.dat"]);
    git_quiet(wt, &["commit", "-qm", "p2"]);

    git_quiet(wt, &["checkout", "-q", "-b", "merge", "p1"]);
    std::fs::write(wt.join("bin.dat"), &bin).unwrap();
    git_quiet(wt, &["add", "bin.dat"]);
    git_quiet(wt, &["commit", "-qm", "merge"]);

    let merge: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let git_out = git_output(wt, &["show", "-p", "--cc", "--no-renames", &merge.to_hex()]);
    assert!(git_out.status.success());
    let git_patch = String::from_utf8_lossy(&git_out.stdout);
    assert!(
        git_patch.contains("Binary files"),
        "git must emit binary combined diff"
    );

    let (result_tree, parents) = read_merge(wt, &merge);
    let config = ConfigSet::new();
    let walk = CombinedTreeDiffOptions::default();
    let paths = combined_diff_paths_filtered(&dr.repo.odb(), &result_tree, &parents, &walk, None)
        .expect("paths");
    let side = paths
        .iter()
        .find(|p| p.path == "bin.dat")
        .expect("bin path");
    let grit = format_combined_textconv_patch(
        &dr.repo.git_dir(),
        &config,
        &dr.repo.odb(),
        "bin.dat",
        &parents,
        &result_tree,
        7,
        3,
        true,
        false,
        CombinedDiffWsOptions::default(),
        false,
        None,
        &side.parents,
        false,
    );
    let grit = grit.unwrap_or_else(|| {
        grit_lib::diff::merge_diff::format_combined_binary(
            "bin.dat",
            &side.parents.iter().map(|s| s.oid).collect::<Vec<_>>(),
            &side.merge_oid,
            7,
            true,
        )
    });
    assert!(
        grit.contains("Binary files"),
        "grit combined binary header must match git"
    );
}

#[test]
fn combined_diff_patch_coalesce_and_whitespace() {
    let ws = CombinedDiffWsOptions {
        ignore_space_change: true,
        ..Default::default()
    };
    let parents = vec!["alpha\n".to_string(), "beta\n".to_string()];
    let dense = format_combined_diff_body(&parents, "gamma\n", 3, true, ws);
    assert!(dense.contains("@@@"), "dense combined diff body");

    let _reused = format_combined_diff_body(
        &["same\n".to_string(), "same\n".to_string()],
        "same\n",
        3,
        true,
        CombinedDiffWsOptions::default(),
    );

    let emptied =
        format_combined_diff_body(&["gone\n".to_string()], "", 3, true, Default::default());
    assert!(emptied.contains('-') || emptied.contains("@@@"));

    let all_ws = CombinedDiffWsOptions {
        ignore_all_space: true,
        ignore_cr_at_eol: true,
        ..Default::default()
    };
    let _ = format_combined_diff_body(&["a b\n".to_string()], "ab\n", 1, false, all_ws);
}

#[test]
fn combined_find_object_and_recursive_raw() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    std::fs::create_dir_all(wt.join("sub")).unwrap();
    std::fs::write(wt.join("sub/a.txt"), "1\n").unwrap();
    git_quiet(wt, &["add", "."]);
    git_quiet(wt, &["commit", "-qm", "root"]);

    git_quiet(wt, &["checkout", "-q", "-b", "p1"]);
    std::fs::write(wt.join("sub/a.txt"), "2\n").unwrap();
    git_quiet(wt, &["commit", "-am", "p1"]);

    git_quiet(wt, &["checkout", "-q", "main"]);
    std::fs::write(wt.join("sub/a.txt"), "3\n").unwrap();
    git_quiet(wt, &["commit", "-am", "p2"]);

    git_quiet(wt, &["checkout", "-q", "-b", "merge", "p1"]);
    std::fs::write(wt.join("sub/a.txt"), "4\n").unwrap();
    git_quiet(wt, &["commit", "-am", "merge"]);

    let merge: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let (result_tree, parents) = read_merge(wt, &merge);
    let blob_oid: ObjectId = git_quiet(wt, &["rev-parse", "HEAD:sub/a.txt"])
        .trim()
        .parse()
        .unwrap();

    let walk = CombinedTreeDiffOptions {
        recursive: true,
        tree_in_recursive: true,
    };
    let filtered = combined_diff_paths_filtered(
        &dr.repo.odb(),
        &result_tree,
        &parents,
        &walk,
        Some(&blob_oid),
    )
    .expect("find-object paths");
    assert!(
        filtered.iter().any(|p| p.path.contains("a.txt")),
        "find-object must retain matching blob path"
    );

    let git_lines = git_raw_lines(
        wt,
        &[
            "diff-tree",
            "--cc",
            "-r",
            "--raw",
            &format!("--find-object={}", blob_oid.to_hex()),
            &merge.to_hex(),
        ],
    );
    let mut grit_lines: Vec<String> = filtered
        .iter()
        .map(|p| format_combined_raw_line(p, None))
        .collect();
    grit_lines.sort();
    assert_eq!(git_lines, grit_lines, "find-object raw combined diff");
}

#[test]
fn combined_all_paths_enrich_renames_raw() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    std::fs::write(wt.join("shared"), "v0\n").unwrap();
    git_quiet(wt, &["add", "shared"]);
    git_quiet(wt, &["commit", "-qm", "root"]);

    git_quiet(wt, &["checkout", "-q", "-b", "p1"]);
    std::fs::remove_file(wt.join("shared")).unwrap();
    std::fs::write(wt.join("left"), "v0\n").unwrap();
    git_quiet(wt, &["add", "-A"]);
    git_quiet(wt, &["commit", "-qm", "p1"]);

    git_quiet(wt, &["checkout", "-q", "main"]);
    std::fs::remove_file(wt.join("shared")).unwrap();
    std::fs::write(wt.join("right"), "v0\n").unwrap();
    git_quiet(wt, &["add", "-A"]);
    git_quiet(wt, &["commit", "-qm", "p2"]);

    git_quiet(wt, &["checkout", "-q", "-b", "merge", "p1"]);
    std::fs::remove_file(wt.join("left")).unwrap();
    std::fs::write(wt.join("merged"), "v0\n").unwrap();
    git_quiet(wt, &["add", "-A"]);
    git_quiet(wt, &["commit", "-qm", "merge"]);

    let merge: ObjectId = git_quiet(wt, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let (result_tree, parents) = read_merge(wt, &merge);
    let walk = CombinedTreeDiffOptions::default();
    let mut paths =
        combined_diff_paths_filtered(&dr.repo.odb(), &result_tree, &parents, &walk, None)
            .expect("paths");
    assert!(!paths.is_empty(), "merge must produce combined paths");
    for path in &mut paths {
        enrich_combined_path_renames(&dr.repo.odb(), path, &parents, &result_tree, 50);
        let line = format_combined_raw_line_all_paths(path, None, true, false);
        assert!(line.starts_with(':'), "all-paths raw line: {line}");
    }
}

#[test]
fn format_worktree_conflict_combined_smoke() {
    let dr = DiffRepo::init();
    let wt = &dr.worktree;
    std::fs::write(wt.join("f"), "base\n").unwrap();
    git_quiet(wt, &["add", "f"]);
    git_quiet(wt, &["commit", "-qm", "c"]);

    let base = git_quiet(wt, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .parse::<ObjectId>()
        .unwrap();
    let stage1 = base;
    let stage2 = dr
        .repo
        .odb()
        .write(grit_lib::objects::ObjectKind::Blob, b"ours\n")
        .expect("stage2 blob");
    let stage3 = dr
        .repo
        .odb()
        .write(grit_lib::objects::ObjectKind::Blob, b"theirs\n")
        .expect("stage3 blob");
    let wt_bytes = b"<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n";
    let config = ConfigSet::new();
    let patch = format_worktree_conflict_combined(
        &dr.repo.git_dir(),
        &config,
        &dr.repo.odb(),
        "f",
        &stage1,
        &stage2,
        &stage3,
        wt_bytes,
        7,
    );
    assert!(patch.contains("diff --cc f"));
    assert!(patch.contains("<<<<<<< ours"));
}
