//! `git merge-file -p` oracle for [`grit_lib::diff::merge_file`].

use std::process::Command;

use grit_lib::diff::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput, MergeOutput};

fn git_merge_file(args: &[&str], base: &[u8], ours: &[u8], theirs: &[u8]) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("base"), base).expect("base");
    std::fs::write(dir.path().join("ours"), ours).expect("ours");
    std::fs::write(dir.path().join("theirs"), theirs).expect("theirs");
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["merge-file", "-p"])
        .args(args)
        .args([
            "-L", "ours", "-L", "base", "-L", "theirs", "ours", "base", "theirs",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git merge-file");
    assert!(
        out.status.success() || out.status.code() == Some(1),
        "git merge-file failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn grit_merge(input: MergeInput<'_>) -> MergeOutput {
    merge(&input).expect("grit merge")
}

fn assert_merge_file_matches_git(
    args: &[&str],
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    style: ConflictStyle,
    favor: MergeFavor,
    marker_size: usize,
    context: &str,
) {
    let git_out = git_merge_file(args, base, ours, theirs);
    let grit_out = grit_merge(MergeInput {
        base,
        ours,
        theirs,
        label_ours: "ours",
        label_base: "base",
        label_theirs: "theirs",
        favor,
        style,
        marker_size,
        diff_algorithm: None,
        ignore_all_space: args.contains(&"-w"),
        ignore_space_change: args.contains(&"-b"),
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    });
    assert_eq!(
        grit_out.content, git_out,
        "{context}: merged content mismatch"
    );
}

#[test]
fn t6427_diff3_and_zdiff3_markers_match_git_merge_file() {
    let base = b"1\n2\n3\n4\n5\n";
    let ours = b"1\n2\nours\n4\n5\n";
    let theirs = b"1\n2\ntheirs\n4\n5\n";
    assert_merge_file_matches_git(
        &[],
        base,
        ours,
        theirs,
        ConflictStyle::Merge,
        MergeFavor::None,
        7,
        "merge style",
    );
    assert_merge_file_matches_git(
        &["--diff3"],
        base,
        ours,
        theirs,
        ConflictStyle::Diff3,
        MergeFavor::None,
        7,
        "diff3",
    );
    assert_merge_file_matches_git(
        &["--zdiff3"],
        base,
        ours,
        theirs,
        ConflictStyle::ZealousDiff3,
        MergeFavor::None,
        7,
        "zdiff3",
    );
}

#[test]
fn t6417_merge_union_and_marker_size_match_git_merge_file() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nOURS\nc\n";
    let theirs = b"a\nTHEIRS\nc\n";
    assert_merge_file_matches_git(
        &["--union"],
        base,
        ours,
        theirs,
        ConflictStyle::Merge,
        MergeFavor::Union,
        7,
        "union",
    );
    assert_merge_file_matches_git(
        &["--marker-size=9"],
        base,
        ours,
        theirs,
        ConflictStyle::Merge,
        MergeFavor::None,
        9,
        "marker size 9",
    );
}

#[test]
fn t6417_merge_favor_ours_theirs_grit_api() {
    let base = b"a\nb\nc\n";
    let ours = b"a\nOURS\nc\n";
    let theirs = b"a\nTHEIRS\nc\n";
    let ours_out = grit_merge(MergeInput {
        base,
        ours,
        theirs,
        label_ours: "ours",
        label_base: "base",
        label_theirs: "theirs",
        favor: MergeFavor::Ours,
        style: ConflictStyle::Merge,
        marker_size: 7,
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: false,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    });
    assert_eq!(ours_out.content, b"a\nOURS\nc\n");
    let theirs_out = grit_merge(MergeInput {
        base,
        ours,
        theirs,
        label_ours: "ours",
        label_base: "base",
        label_theirs: "theirs",
        favor: MergeFavor::Theirs,
        style: ConflictStyle::Merge,
        marker_size: 7,
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: false,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    });
    assert_eq!(theirs_out.content, b"a\nTHEIRS\nc\n");
}

/// Issue #2259: CRLF conversion on one side plus an edit on the other must conflict (not auto-merge).
#[test]
fn issue_2259_crlf_conversion_with_edit_conflicts_like_git() {
    let base = b"a\nb\nc\nd\n";
    let ours = b"a\r\nb\r\nc\r\nd\r\n";
    let theirs = b"a\nB\nc\nd\n";
    assert_merge_file_matches_git(
        &[],
        base,
        ours,
        theirs,
        ConflictStyle::Merge,
        MergeFavor::None,
        7,
        "issue 2259 interior edit",
    );
    let grit_out = grit_merge(MergeInput {
        base,
        ours,
        theirs,
        label_ours: "ours",
        label_base: "base",
        label_theirs: "theirs",
        favor: MergeFavor::None,
        style: ConflictStyle::Merge,
        marker_size: 7,
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: false,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    });
    assert_eq!(grit_out.conflicts, 1, "expected one conflict region");
}

#[test]
fn issue_2259_crlf_conversion_with_trailing_append_conflicts_like_git() {
    let base = b"a\nb\nc\nd\n";
    let ours = b"a\r\nb\r\nc\r\nd\r\n";
    let theirs = b"a\nb\nc\nd\ne\n";
    assert_merge_file_matches_git(
        &[],
        base,
        ours,
        theirs,
        ConflictStyle::Merge,
        MergeFavor::None,
        7,
        "issue 2259 trailing append",
    );
}

#[test]
fn issue_2259_adjacent_lf_edits_still_conflict() {
    let base = b"a\nb\nc\nd\n";
    let ours = b"a\nB\nc\nd\n";
    let theirs = b"a\nb\nC\nd\n";
    assert_merge_file_matches_git(
        &[],
        base,
        ours,
        theirs,
        ConflictStyle::Merge,
        MergeFavor::None,
        7,
        "adjacent single-line LF edits",
    );
}

#[test]
fn issue_2259_non_adjacent_crlf_edits_merge_clean() {
    let base = b"a\nb\nc\nd\n";
    let ours = b"a\r\nb\r\nc\r\nd\r\n";
    let theirs = b"a\nb\nC\nd\n";
    assert_merge_file_matches_git(
        &[],
        base,
        ours,
        theirs,
        ConflictStyle::Merge,
        MergeFavor::None,
        7,
        "non-adjacent CRLF vs LF edit",
    );
}

#[test]
fn issue_2259_identical_crlf_conversion_merge_clean() {
    let base = b"a\nb\nc\nd\n";
    let ours = b"a\r\nb\r\nc\r\nd\r\n";
    let theirs = b"a\r\nb\r\nc\r\nd\r\n";
    assert_merge_file_matches_git(
        &[],
        base,
        ours,
        theirs,
        ConflictStyle::Merge,
        MergeFavor::None,
        7,
        "identical CRLF on both sides",
    );
}

#[test]
fn issue_2259_incomplete_line_lone_cr_conflicts_like_git() {
    let base = b"a\n";
    let ours = b"a\r";
    let theirs = b"A\n";
    assert_merge_file_matches_git(
        &[],
        base,
        ours,
        theirs,
        ConflictStyle::Merge,
        MergeFavor::None,
        7,
        "incomplete final line with lone CR on ours",
    );
    let grit_out = grit_merge(MergeInput {
        base,
        ours,
        theirs,
        label_ours: "ours",
        label_base: "base",
        label_theirs: "theirs",
        favor: MergeFavor::None,
        style: ConflictStyle::Merge,
        marker_size: 7,
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: false,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    });
    assert_eq!(grit_out.conflicts, 1);
    assert!(
        grit_out.content.windows(2).any(|w| w == b"a\r"),
        "ours side must preserve lone CR"
    );
}

#[test]
fn issue_2259_ignore_cr_at_eol_clean_for_interior_edit() {
    let base = b"a\nb\nc\nd\n";
    let ours = b"a\r\nb\r\nc\r\nd\r\n";
    let theirs = b"a\nB\nc\nd\n";
    let grit_out = grit_merge(MergeInput {
        base,
        ours,
        theirs,
        label_ours: "ours",
        label_base: "base",
        label_theirs: "theirs",
        favor: MergeFavor::None,
        style: ConflictStyle::Merge,
        marker_size: 7,
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: false,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: true,
    });
    assert_eq!(
        grit_out.conflicts, 0,
        "ignore_cr_at_eol should auto-merge CR-only EOL diffs"
    );
}
