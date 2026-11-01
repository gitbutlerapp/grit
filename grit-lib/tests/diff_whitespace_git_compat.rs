//! Whitespace diff modes vs system `git diff` (t4015 / t4040 semantics).

use crate::support;

use grit_lib::diff::{line_diff_unified_hunk_body, DiffAlgorithm, DiffOptions, WhitespaceMode};
use support::{
    git_diff_no_index_patch, patch_hunk_body, LineDiffAlgo, WhitespaceMode as HarnessWs,
};

fn git_hunks(old: &str, new: &str, algo: LineDiffAlgo, ws: HarnessWs) -> String {
    patch_hunk_body(&git_diff_no_index_patch(
        old.as_bytes(),
        new.as_bytes(),
        "f",
        algo,
        ws,
        &[],
    ))
}

fn grit_hunks(old: &str, new: &str, algo: DiffAlgorithm, ws: WhitespaceMode) -> String {
    let opts = DiffOptions::default()
        .with_whitespace(ws)
        .with_algorithm(algo);
    let body = line_diff_unified_hunk_body(old, new, &opts);
    if body.is_empty() {
        return String::new();
    }
    patch_hunk_body(&format!("placeholder\n{body}"))
}

#[test]
fn ignore_all_space_matches_git() {
    let old = "hello world\n";
    let new = "helloworld\n";
    assert_eq!(
        grit_hunks(
            old,
            new,
            DiffAlgorithm::Myers,
            WhitespaceMode::ignore_all_space()
        ),
        git_hunks(old, new, LineDiffAlgo::Myers, HarnessWs::IgnoreAllSpace)
    );
}

#[test]
fn ignore_space_change_matches_git() {
    let old = "foo bar\n";
    let new = "foo  bar\n";
    assert_eq!(
        grit_hunks(
            old,
            new,
            DiffAlgorithm::Myers,
            WhitespaceMode::ignore_space_change()
        ),
        git_hunks(old, new, LineDiffAlgo::Myers, HarnessWs::IgnoreSpaceChange)
    );
}

#[test]
fn ignore_blank_lines_matches_git() {
    let old = "a\n\nb\n";
    let new = "a\nb\n";
    assert_eq!(
        grit_hunks(
            old,
            new,
            DiffAlgorithm::Myers,
            WhitespaceMode::ignore_blank_lines()
        ),
        git_hunks(old, new, LineDiffAlgo::Myers, HarnessWs::IgnoreBlankLines)
    );
}

#[test]
fn whitespace_only_file_omitted_like_git() {
    let old = "same\n";
    let new = "s a m e\n";
    let opts = DiffOptions::default().with_whitespace(WhitespaceMode::ignore_all_space());
    assert!(
        line_diff_unified_hunk_body(old, new, &opts).is_empty(),
        "whitespace-only change produces no hunks"
    );
    assert!(
        git_hunks(old, new, LineDiffAlgo::Myers, HarnessWs::IgnoreAllSpace).is_empty(),
        "git also omits patch"
    );
}

#[test]
fn ignore_all_space_nbsp_still_differs_like_git() {
    let old = "a\u{00a0}b\n";
    let new = "ab\n";
    let ws = WhitespaceMode::ignore_all_space();
    let opts = DiffOptions::default().with_whitespace(ws);
    assert!(
        !line_diff_unified_hunk_body(old, new, &opts).is_empty(),
        "NBSP removal must not be treated as ignorable -w whitespace"
    );
    assert_eq!(
        grit_hunks(old, new, DiffAlgorithm::Myers, ws),
        git_hunks(old, new, LineDiffAlgo::Myers, HarnessWs::IgnoreAllSpace)
    );
}

#[test]
fn ignore_blank_lines_respects_myers_not_histogram() {
    let old = (0..5)
        .map(|i| format!("{}\n", char::from(b'A' + i)))
        .collect::<String>();
    let new = (0..5)
        .map(|i| format!("{}\n", char::from(b'A' + i)))
        .collect::<String>();
    let mut opts = DiffOptions::default().with_whitespace(WhitespaceMode::ignore_blank_lines());
    opts.algorithm = DiffAlgorithm::Myers;
    let body = line_diff_unified_hunk_body(&old, &new, &opts);
    assert!(
        body.is_empty(),
        "identical non-blank lines should produce no hunks"
    );
    let git_patch = git_diff_no_index_patch(
        old.as_bytes(),
        new.as_bytes(),
        "f",
        LineDiffAlgo::Myers,
        HarnessWs::IgnoreBlankLines,
        &[],
    );
    assert!(
        patch_hunk_body(&git_patch).is_empty(),
        "git also shows no hunks for identical content with --ignore-blank-lines"
    );
}

#[test]
fn ignore_all_space_eof_newline_matches_git() {
    let old = "line\n";
    let new = "line";
    let ws = WhitespaceMode::ignore_all_space();
    assert_eq!(
        grit_hunks(old, new, DiffAlgorithm::Myers, ws),
        git_hunks(old, new, LineDiffAlgo::Myers, HarnessWs::IgnoreAllSpace),
        "git -w treats missing final newline like other whitespace-only edits"
    );
}

#[test]
fn ignore_all_space_patience_compaction_matches_git() {
    let old = "A\nsame \nA\nD\nD \nB \nsame \nB \nB \n";
    let new = "A\nyZ\nA \nD\nB\nsame\nB\n";
    let ws = HarnessWs::IgnoreAllSpace;
    let grit_ws = WhitespaceMode::ignore_all_space();
    assert_eq!(
        grit_hunks(old, new, DiffAlgorithm::Patience, grit_ws),
        git_hunks(old, new, LineDiffAlgo::Patience, ws),
        "patience -w must use whitespace-aware compaction (second D line deleted, not first)"
    );
}

#[test]
fn whitespace_modes_with_crlf_match_git() {
    let old = "line one\r\nline two\r\n";
    let new = "line  one\r\nline two\r\n";
    assert_eq!(
        grit_hunks(
            old,
            new,
            DiffAlgorithm::Myers,
            WhitespaceMode::ignore_space_change()
        ),
        git_hunks(old, new, LineDiffAlgo::Myers, HarnessWs::IgnoreSpaceChange)
    );
}

#[test]
fn ignore_all_space_histogram_keeps_shared_line_as_context() {
    let old = "same\nold\n";
    let new = "same\nnew\n";
    let ws = WhitespaceMode::ignore_all_space();
    assert_eq!(
        grit_hunks(old, new, DiffAlgorithm::Histogram, ws),
        git_hunks(old, new, LineDiffAlgo::Histogram, HarnessWs::IgnoreAllSpace),
        "histogram -w must keep unchanged `same` as context, not as removed+added"
    );
}

#[test]
fn ignore_all_space_histogram_three_line_patch_no_panic() {
    let old = "old-before\nsame\nold-after\n";
    let new = "new-before\nsame\nnew-after\n";
    let ws = WhitespaceMode::ignore_all_space();
    assert_eq!(
        grit_hunks(old, new, DiffAlgorithm::Histogram, ws),
        git_hunks(old, new, LineDiffAlgo::Histogram, HarnessWs::IgnoreAllSpace),
        "histogram -w three-line change must match git without panicking"
    );
}

/// Repeated lines with `-w` normalization: grit must match system git per algorithm.
#[test]
fn ignore_all_space_histogram_matches_git_oracle() {
    let old = "A x\nx y\nA x\nA x\nA x\nx y\nA x\nA x\nA x\n";
    let new = "A  x\nx  y\nA  x\nZ q\nA  x\nx  y\nA  x\nA  x\nA  x\n";
    let ws = HarnessWs::IgnoreAllSpace;
    let grit_ws = WhitespaceMode::ignore_all_space();

    let git_myers = git_hunks(old, new, LineDiffAlgo::Myers, ws);
    let git_histogram = git_hunks(old, new, LineDiffAlgo::Histogram, ws);

    assert_eq!(
        grit_hunks(old, new, DiffAlgorithm::Myers, grit_ws),
        git_myers,
        "grit Myers -w hunk body must match git Myers"
    );
    assert_eq!(
        grit_hunks(old, new, DiffAlgorithm::Histogram, grit_ws),
        git_histogram,
        "grit Histogram -w hunk body must match git Histogram"
    );
}

#[test]
fn ignore_space_change_histogram_matches_git_oracle() {
    let old = "A x\nx y\nA x\nA x\nA x\nx y\nA x\nA x\nA x\n";
    let new = "A  x\nx  y\nA  x\nZ q\nA  x\nx  y\nA  x\nA  x\nA  x\n";
    let ws = HarnessWs::IgnoreSpaceChange;
    let grit_ws = WhitespaceMode::ignore_space_change();

    let git_myers = git_hunks(old, new, LineDiffAlgo::Myers, ws);
    let git_histogram = git_hunks(old, new, LineDiffAlgo::Histogram, ws);

    assert_eq!(
        grit_hunks(old, new, DiffAlgorithm::Myers, grit_ws),
        git_myers,
        "grit Myers -b hunk body must match git Myers"
    );
    assert_eq!(
        grit_hunks(old, new, DiffAlgorithm::Histogram, grit_ws),
        git_histogram,
        "grit Histogram -b hunk body must match git Histogram"
    );
}

/// Histogram change-compact can merge groups and Myers-rediff the slice; rediff must honor `-w`.
#[test]
fn ignore_all_space_histogram_merged_group_rediff_matches_git() {
    let old = "x \nx\nx\nA\nB\nC\nx \nD\nE\n";
    let new = "x\nx\nx\nA\nB\nC\nx\nD\nE\n";
    let ws = WhitespaceMode::ignore_all_space();
    assert_eq!(
        grit_hunks(old, new, DiffAlgorithm::Histogram, ws),
        git_hunks(old, new, LineDiffAlgo::Histogram, HarnessWs::IgnoreAllSpace),
        "histogram -w rediff after compact must match system git"
    );
}

fn po_translation_fixture() -> (String, String) {
    let mut old = String::new();
    let mut new = String::new();
    for i in 0..6000 {
        use std::fmt::Write as _;
        let _ = writeln!(old, "#: file{}.c:{}", i % 300, i % 97);
        let _ = writeln!(old, "msgid \"message {i}\"");
        let _ = writeln!(old, "msgstr \"\"");
        old.push('\n');
        let msg_new = if i % 7 == 0 { i + 100_000 } else { i };
        let _ = writeln!(new, "#: file{}.c:{}", i % 300, i % 97);
        let _ = writeln!(new, "msgid \"message {msg_new}\"");
        let _ = writeln!(new, "msgstr \"\"");
        new.push('\n');
    }
    (old, new)
}

/// Issue #2451: repeated-line po files — histogram numstat must match system git.
#[test]
fn repeated_line_po_histogram_numstat_matches_git() {
    let (old, new) = po_translation_fixture();
    let git = support::git_numstat(
        old.as_bytes(),
        new.as_bytes(),
        "a.po",
        support::LineDiffAlgo::Histogram,
    );
    let grit = support::grit_numstat(&old, &new, support::LineDiffAlgo::Histogram);
    assert_eq!(grit, git, "histogram numstat must match git on po fixture");
}

#[test]
fn repeated_line_file_histogram_numstat_matches_git() {
    let old = (0..800)
        .flat_map(|i| {
            [
                format!("#: x.c:{i}"),
                format!("msgid \"{i}\""),
                "msgstr \"\"".into(),
                String::new(),
            ]
        })
        .collect::<Vec<_>>()
        .join("\n");
    let new = (0..800)
        .flat_map(|i| {
            let v = if i % 11 == 0 { i + 999 } else { i };
            [
                format!("#: x.c:{i}"),
                format!("msgid \"{v}\""),
                "msgstr \"\"".into(),
                String::new(),
            ]
        })
        .collect::<Vec<_>>()
        .join("\n");
    let git = support::git_numstat(
        old.as_bytes(),
        new.as_bytes(),
        "f.po",
        support::LineDiffAlgo::Histogram,
    );
    let grit = support::grit_numstat(&old, &new, support::LineDiffAlgo::Histogram);
    assert_eq!(grit, git);
}
