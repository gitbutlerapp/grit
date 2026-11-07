//! `unified_diff` / `line_change_counts` with [`UnifiedDiffOptions`] against system
//! `git diff --no-index`: the patch from the `---` line on must be byte-identical, and the
//! counts must equal `git diff --numstat`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::ConfigSet;
use grit_lib::diff::userdiff::matcher_for_driver;
use grit_lib::diff::{
    line_change_counts, unified_diff, DiffAlgorithm, DiffLabel, HunkExtent, IndentHeuristic,
    NoNewlineMarker, PathQuoting, UnifiedDiffOptions, WhitespaceMode,
};

const NULL_CONFIG: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// `git` in `dir` with no system, global or environment-injected configuration.
fn hermetic_git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", NULL_CONFIG)
        .env("GIT_CONFIG_SYSTEM", NULL_CONFIG)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env("LC_ALL", "C");
    cmd
}

/// Runs `git diff --no-index` on `old` / `new` written to files of those names in a temp dir.
fn git_no_index(old: &str, new: &str, names: (&str, &str), args: &[&str]) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join(names.0), old).expect("write old");
    fs::write(dir.path().join(names.1), new).expect("write new");
    git_no_index_in(dir.path(), names, args)
}

fn git_no_index_in(dir: &Path, names: (&str, &str), args: &[&str]) -> String {
    let out = hermetic_git(dir)
        .args([
            "-c",
            "core.attributesFile=",
            "diff",
            "--no-index",
            "--no-color",
        ])
        .args(args)
        .arg("--")
        .args([names.0, names.1])
        .output()
        .expect("spawn git");
    assert!(
        matches!(out.status.code(), Some(0 | 1)),
        "git diff failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 patch")
}

/// Git's patch without the `diff --git` and `index` lines, which name blobs grit never sees.
fn from_label_lines(patch: &str) -> String {
    match patch.find("\n--- ") {
        Some(at) => patch[at + 1..].to_owned(),
        None => {
            assert!(patch.is_empty(), "unexpected git output:\n{patch}");
            String::new()
        }
    }
}

fn assert_matches_git(
    case: &str,
    old: &str,
    new: &str,
    args: &[&str],
    options: &UnifiedDiffOptions,
) {
    let names = ("old.txt", "new.txt");
    let git = from_label_lines(&git_no_index(old, new, names, args));
    let options = options.clone().with_paths(names.0, names.1);
    let grit = unified_diff(old, new, &options);
    assert_eq!(
        grit,
        git,
        "{case}: `git diff --no-index {}`",
        args.join(" ")
    );
}

fn assert_counts_match_git(
    case: &str,
    old: &str,
    new: &str,
    args: &[&str],
    options: &UnifiedDiffOptions,
) {
    let mut numstat_args = vec!["--numstat"];
    numstat_args.extend_from_slice(args);
    let git = git_no_index(old, new, ("old.txt", "new.txt"), &numstat_args);
    let mut fields = git.split('\t');
    let git_counts = match (fields.next(), fields.next()) {
        (Some(ins), Some(del)) => (ins.parse().unwrap(), del.parse().unwrap()),
        _ => (0, 0),
    };
    let stat = line_change_counts(old, new, options);
    assert_eq!(
        (stat.insertions, stat.deletions),
        git_counts,
        "{case}: `git diff --numstat {}`",
        args.join(" ")
    );
}

struct Fixture {
    name: &'static str,
    old: String,
    new: String,
}

fn fixture(name: &'static str, old: &str, new: &str) -> Fixture {
    Fixture {
        name,
        old: old.to_owned(),
        new: new.to_owned(),
    }
}

const C_OLD: &str = "#include <stdio.h>

static int add(int a, int b)
{
\treturn a + b;
}

static int sub(int a, int b)
{
\treturn a - b;
}

int main(void)
{
\tint x = add(1, 2);
\tint y = sub(5, 3);
\tprintf(\"%d %d\\n\", x, y);
\treturn 0;
}
";

const C_NEW: &str = "#include <stdio.h>
#include <stdlib.h>

static int add(int a, int b)
{
\tint sum = a + b;
\treturn sum;
}

static int mul(int a, int b)
{
\treturn a * b;
}

static int sub(int a, int b)
{
\treturn a - b;
}

int main(void)
{
\tint x = add(1, 2);
\tint y = sub(5, 3);
\tint z = mul(x, y);
\tprintf(\"%d %d %d\\n\", x, y, z);
\treturn 0;
}
";

/// Forty numbered lines with edits at lines 5, 14 and 30 (gaps of 8 and 15 unchanged lines).
fn spaced_edits() -> Fixture {
    let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    let new: String = (1..=40)
        .map(|i| match i {
            5 => "line five\n".to_owned(),
            14 => "line fourteen\nline fourteen and a half\n".to_owned(),
            30 => String::new(),
            _ => format!("line {i}\n"),
        })
        .collect();
    Fixture {
        name: "spaced-edits",
        old,
        new,
    }
}

fn fixtures() -> Vec<Fixture> {
    vec![
        fixture("c-functions", C_OLD, C_NEW),
        fixture(
            "repeated-lines",
            "a\nb\nc\na\nb\nc\nX\na\nb\nc\n",
            "a\nb\nX\nc\na\nb\nc\na\nc\nb\n",
        ),
        fixture(
            "moved-function",
            "fn one() {\n    1\n}\n\nfn two() {\n    2\n}\n\nfn three() {\n    3\n}\n",
            "fn three() {\n    3\n}\n\nfn one() {\n    1\n}\n\nfn two() {\n    22\n}\n",
        ),
        fixture(
            "braces",
            "if (a) {\n\tx();\n}\nif (b) {\n\ty();\n}\nif (c) {\n\tz();\n}\n",
            "if (a) {\n\tx();\n}\nif (bb) {\n\ty();\n\tw();\n}\nif (c) {\n\tz();\n}\n",
        ),
        fixture("append-without-newline", "a\nb\n", "a\nb\nc"),
        fixture("drop-final-newline", "a\nb\nc\n", "a\nb\nc"),
        fixture("change-last-line-no-newline", "a\nb\nc", "a\nb\nC"),
        fixture("into-empty", "only\nlines\n", ""),
        fixture("from-empty", "", "fresh\ncontent\n"),
        spaced_edits(),
    ]
}

fn algorithm_args(algorithm: DiffAlgorithm) -> &'static [&'static str] {
    match algorithm {
        DiffAlgorithm::Histogram => &["--histogram"],
        DiffAlgorithm::Patience => &["--patience"],
        DiffAlgorithm::Minimal => &["--minimal"],
        _ => &[],
    }
}

const ALGORITHMS: [DiffAlgorithm; 4] = [
    DiffAlgorithm::Myers,
    DiffAlgorithm::Minimal,
    DiffAlgorithm::Patience,
    DiffAlgorithm::Histogram,
];

#[test]
fn default_options_match_git() {
    for f in fixtures() {
        let options = UnifiedDiffOptions::new();
        assert_matches_git(f.name, &f.old, &f.new, &[], &options);
        assert_counts_match_git(f.name, &f.old, &f.new, &[], &options);
    }
}

#[test]
fn every_algorithm_matches_git() {
    for algorithm in ALGORITHMS {
        let options = UnifiedDiffOptions::new().with_algorithm(algorithm);
        for f in fixtures() {
            let case = format!("{} {algorithm:?}", f.name);
            assert_matches_git(&case, &f.old, &f.new, algorithm_args(algorithm), &options);
            assert_counts_match_git(&case, &f.old, &f.new, algorithm_args(algorithm), &options);
        }
    }
}

#[test]
fn indent_heuristic_off_matches_git() {
    for f in fixtures() {
        let options = UnifiedDiffOptions::new().with_indent_heuristic(IndentHeuristic::Disabled);
        assert_matches_git(f.name, &f.old, &f.new, &["--no-indent-heuristic"], &options);
    }
}

#[test]
fn zero_context_matches_git() {
    for algorithm in ALGORITHMS {
        let options = UnifiedDiffOptions::new()
            .with_algorithm(algorithm)
            .with_context_lines(0);
        let mut args = vec!["-U0"];
        args.extend_from_slice(algorithm_args(algorithm));
        for f in fixtures() {
            let case = format!("{} {algorithm:?}", f.name);
            assert_matches_git(&case, &f.old, &f.new, &args, &options);
        }
    }
}

#[test]
fn inter_hunk_context_matches_git() {
    let f = spaced_edits();
    for (context, inter_hunk) in [
        (3, 0),
        (3, 1),
        (3, 2),
        (3, 8),
        (1, 3),
        (0, 7),
        (0, 8),
        (2, 20),
    ] {
        let options = UnifiedDiffOptions::new()
            .with_context_lines(context)
            .with_inter_hunk_context(inter_hunk);
        let u = format!("-U{context}");
        let ihc = format!("--inter-hunk-context={inter_hunk}");
        let case = format!("{} {u} {ihc}", f.name);
        assert_matches_git(&case, &f.old, &f.new, &[&u, &ihc], &options);
    }
    for f in fixtures() {
        let options = UnifiedDiffOptions::new()
            .with_algorithm(DiffAlgorithm::Histogram)
            .with_inter_hunk_context(4);
        let args = ["--histogram", "--inter-hunk-context=4"];
        assert_matches_git(f.name, &f.old, &f.new, &args, &options);
    }
}

#[test]
fn default_funcname_rule_matches_git() {
    let old: String = std::iter::once("struct Point {\n".to_owned())
        .chain((0..8).map(|i| format!("    field{i}: u32,\n")))
        .chain([
            "}\n".to_owned(),
            "\n".to_owned(),
            "impl Point {\n".to_owned(),
        ])
        .chain((0..8).map(|i| format!("    // step {i}\n")))
        .chain([
            "    fn x(&self) -> u32 { 0 }\n".to_owned(),
            "}\n".to_owned(),
        ])
        .collect();
    let new = old
        .replace("field6: u32", "field6: u64")
        .replace("// step 7", "// step seven");
    let options = UnifiedDiffOptions::new();
    assert_matches_git("default funcname", &old, &new, &[], &options);
    assert_matches_git("C file funcname", C_OLD, C_NEW, &[], &options);
}

#[test]
fn userdiff_driver_funcname_matches_git() {
    let old = "impl Shape {\n    pub fn area(&self) -> f64 {\n        let w = self.w;\n        \
               let h = self.h;\n        let pad = 0.0;\n        w * h + pad\n    }\n}\n";
    let new = old.replace("w * h + pad", "w * h");
    let matcher = matcher_for_driver(&ConfigSet::default(), "rust")
        .expect("matcher")
        .expect("rust builtin");

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("old.rs"), old).expect("write");
    fs::write(dir.path().join("new.rs"), &new).expect("write");
    let attributes = dir.path().join("attributes");
    fs::write(&attributes, "*.rs diff=rust\n").expect("write attributes");
    let attributes_arg = format!("core.attributesFile={}", attributes.display());
    let out = hermetic_git(dir.path())
        .args([
            "-c",
            &attributes_arg,
            "diff",
            "--no-index",
            "--no-color",
            "old.rs",
            "new.rs",
        ])
        .output()
        .expect("spawn git");
    let git = from_label_lines(&String::from_utf8(out.stdout).expect("utf-8"));
    assert!(
        git.contains(" @@ pub fn area(&self) -> f64 {\n"),
        "git uses the rust driver:\n{git}"
    );

    let options = UnifiedDiffOptions::new()
        .with_paths("old.rs", "new.rs")
        .with_funcname(&matcher);
    assert_eq!(unified_diff(old, &new, &options), git);

    let default_rule = unified_diff(old, &new, &UnifiedDiffOptions::new());
    assert!(
        default_rule.contains(" @@ impl Shape {\n"),
        "without the driver the default rule picks the unindented line:\n{default_rule}"
    );
}

#[test]
fn anchored_matches_git() {
    let cases: [(&str, &str, &str, &[&str]); 6] = [
        ("t4065 rotate", "a\nb\nc\n", "c\na\nb\n", &["c"]),
        (
            "moved function",
            "fn one() {\n    1\n}\n\nfn two() {\n    2\n}\n\nfn three() {\n    3\n}\n",
            "fn three() {\n    3\n}\n\nfn one() {\n    1\n}\n\nfn two() {\n    22\n}\n",
            &["fn three"],
        ),
        (
            "two anchors",
            "a\nb\nc\nd\ne\nf\n",
            "e\nf\nc\nd\na\nb\n",
            &["a", "c"],
        ),
        ("anchor not unique", "x\nq\nx\nr\n", "r\nx\nq\nx\n", &["x"]),
        (
            "anchors that cross",
            "one\ntwo\nthree\nfour\n",
            "four\nthree\ntwo\none\n",
            &["one", "four"],
        ),
        ("anchor missing", "a\nb\nc\n", "b\nc\na\n", &["zzz"]),
    ];
    for (case, old, new, anchors) in cases {
        let options = UnifiedDiffOptions::new().with_anchors(anchors.iter().copied());
        let args: Vec<String> = anchors.iter().map(|a| format!("--anchored={a}")).collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_matches_git(case, old, new, &args, &options);
        assert_counts_match_git(case, old, new, &args, &options);
    }
    let c_anchor = UnifiedDiffOptions::new().with_anchors(["static int mul"]);
    assert_matches_git(
        "C mul",
        C_OLD,
        C_NEW,
        &["--anchored=static int mul"],
        &c_anchor,
    );
}

#[test]
fn anchors_override_the_algorithm_like_git() {
    let options = UnifiedDiffOptions::new()
        .with_algorithm(DiffAlgorithm::Histogram)
        .with_anchors(["c"]);
    assert_matches_git(
        "histogram then anchored",
        "a\nb\nc\n",
        "c\na\nb\n",
        &["--histogram", "--anchored=c"],
        &options,
    );
}

fn whitespace_fixtures() -> Vec<Fixture> {
    vec![
        fixture(
            "indent and operator spacing",
            "if (a  &&  b) {\n\tfoo();\n}\nbar( x );\nkeep\n",
            "if (a && b) {\n    foo();\n}\nbaz(x);\nkeep\n",
        ),
        fixture(
            "trailing spaces",
            "alpha  \nbeta\ngamma\t\ndelta\n",
            "alpha\nbeta changed\ngamma\ndelta \n",
        ),
        fixture(
            "context shows new side",
            "one\n  two\nthree\nfour\n",
            "one\n    two\nthree\nFOUR\n",
        ),
        fixture("whitespace only", "same  text\n", "same text\n"),
        fixture("blank lines", "a\n\nb\nc\n\n\nd\n", "a\nb\nc\nd\n\ne\n"),
    ]
}

#[test]
fn ignore_space_change_matches_git() {
    for algorithm in ALGORITHMS {
        let options = UnifiedDiffOptions::new()
            .with_algorithm(algorithm)
            .with_whitespace(WhitespaceMode::ignore_space_change());
        let mut args = vec!["-b"];
        args.extend_from_slice(algorithm_args(algorithm));
        for f in whitespace_fixtures().into_iter().chain(fixtures()) {
            let case = format!("{} {algorithm:?}", f.name);
            assert_matches_git(&case, &f.old, &f.new, &args, &options);
            assert_counts_match_git(&case, &f.old, &f.new, &args, &options);
        }
    }
}

#[test]
fn other_whitespace_modes_match_git() {
    let mut at_eol = WhitespaceMode::default();
    at_eol.ignore_space_at_eol = true;
    let modes = [
        (WhitespaceMode::ignore_all_space(), "-w"),
        (at_eol, "--ignore-space-at-eol"),
        (WhitespaceMode::ignore_blank_lines(), "--ignore-blank-lines"),
    ];
    for (mode, flag) in modes {
        for algorithm in [DiffAlgorithm::Myers, DiffAlgorithm::Histogram] {
            let options = UnifiedDiffOptions::new()
                .with_algorithm(algorithm)
                .with_whitespace(mode);
            let mut args = vec![flag];
            args.extend_from_slice(algorithm_args(algorithm));
            for f in whitespace_fixtures() {
                let case = format!("{} {flag} {algorithm:?}", f.name);
                assert_matches_git(&case, &f.old, &f.new, &args, &options);
                assert_counts_match_git(&case, &f.old, &f.new, &args, &options);
            }
        }
    }
}

#[test]
fn function_context_matches_git() {
    let options = UnifiedDiffOptions::new().with_extent(HunkExtent::Function);
    assert_matches_git("C -W", C_OLD, C_NEW, &["-W"], &options);
    let old = "int a(void)\n{\n\treturn 1;\n}\n\nint b(void)\n{\n\tint x = 2;\n\treturn x;\n}\n\nint c(void)\n{\n\treturn 3;\n}\n";
    let new = old.replace("int x = 2;", "int x = 20;");
    assert_matches_git("middle function -W", old, &new, &["-W"], &options);

    let two_functions = new.replace("return 3;", "return 30;");
    assert_matches_git("two functions -W", old, &two_functions, &["-W"], &options);
    let appended = format!("{old}\nint d(void)\n{{\n\treturn 4;\n}}\n");
    assert_matches_git("appended function -W", old, &appended, &["-W"], &options);
    let header_change = format!(
        "/* header */\n{}",
        old.replacen("int a(void)", "long a(void)", 1)
    );
    assert_matches_git(
        "change before functions -W",
        old,
        &header_change,
        &["-W"],
        &options,
    );
    let comment_old = "/*\n * doc\n */\nint a(void)\n{\n\treturn 1;\n}\n";
    let comment_new = comment_old.replace("return 1;", "return 2;");
    assert_matches_git(
        "doc comment -W",
        comment_old,
        &comment_new,
        &["-W"],
        &options,
    );

    let histogram = options.clone().with_algorithm(DiffAlgorithm::Histogram);
    assert_matches_git(
        "C -W histogram",
        C_OLD,
        C_NEW,
        &["-W", "--histogram"],
        &histogram,
    );
    let spaces = options
        .clone()
        .with_whitespace(WhitespaceMode::ignore_space_change());
    let respaced = two_functions.replace("\treturn 1;", "    return  1;");
    assert_matches_git("-W -b", old, &respaced, &["-W", "-b"], &spaces);
    for f in fixtures() {
        assert_matches_git(f.name, &f.old, &f.new, &["-W"], &options);
    }
}

#[test]
fn labels_match_git() {
    let old = "a\n";
    let new = "b\n";
    for (names, quoting, args) in [
        (("my old", "my new"), PathQuoting::EscapeNonAscii, &[][..]),
        (
            ("caf\u{e9}.txt", "na\u{ef}ve.txt"),
            PathQuoting::EscapeNonAscii,
            &[][..],
        ),
        (
            ("caf\u{e9}.txt", "na\u{ef}ve.txt"),
            PathQuoting::KeepNonAscii,
            &["-c", "core.quotePath=false"][..],
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(names.0), old).expect("write");
        fs::write(dir.path().join(names.1), new).expect("write");
        let out = hermetic_git(dir.path())
            .args(args)
            .args(["diff", "--no-index", "--no-color", "--", names.0, names.1])
            .output()
            .expect("spawn git");
        let git = from_label_lines(&String::from_utf8(out.stdout).expect("utf-8"));
        let options = UnifiedDiffOptions::new()
            .with_paths(names.0, names.1)
            .with_path_quoting(quoting);
        assert_eq!(
            unified_diff(old, new, &options),
            git,
            "{names:?} {quoting:?}"
        );
    }

    let git = from_label_lines(&git_no_index(old, new, ("x", "y"), &["--no-prefix"]));
    let options = UnifiedDiffOptions::new().with_labels(
        DiffLabel::Path {
            prefix: "",
            path: "x",
        },
        DiffLabel::Path {
            prefix: "",
            path: "y",
        },
    );
    assert_eq!(unified_diff(old, new, &options), git, "--no-prefix");

    let git = from_label_lines(&git_no_index(
        old,
        new,
        ("x", "y"),
        &["--src-prefix=left/", "--dst-prefix=right/"],
    ));
    let options = UnifiedDiffOptions::new().with_labels(
        DiffLabel::Path {
            prefix: "left/",
            path: "x",
        },
        DiffLabel::Path {
            prefix: "right/",
            path: "y",
        },
    );
    assert_eq!(unified_diff(old, new, &options), git, "custom prefixes");
}

#[test]
fn added_and_deleted_sides_use_dev_null_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("new.txt"), "fresh\n").expect("write");
    let out = hermetic_git(dir.path())
        .args([
            "diff",
            "--no-index",
            "--no-color",
            "--",
            "/dev/null",
            "new.txt",
        ])
        .output()
        .expect("spawn git");
    let git = String::from_utf8(out.stdout).expect("utf-8");
    let git = from_label_lines(&git);
    let options = UnifiedDiffOptions::new().with_labels(
        DiffLabel::DevNull,
        DiffLabel::Path {
            prefix: "b/",
            path: "new.txt",
        },
    );
    assert_eq!(unified_diff("", "fresh\n", &options), git);
}

#[test]
fn hunks_only_and_omitted_markers() {
    let f = fixture("append-without-newline", "a\nb\n", "a\nb\nc");
    let git = from_label_lines(&git_no_index(&f.old, &f.new, ("old.txt", "new.txt"), &[]));
    let hunks_only = git.splitn(3, '\n').nth(2).expect("hunks after labels");
    assert_eq!(
        unified_diff(&f.old, &f.new, &UnifiedDiffOptions::new()),
        hunks_only
    );

    let without_markers = UnifiedDiffOptions::new().with_no_newline_marker(NoNewlineMarker::Omit);
    assert_eq!(
        unified_diff(&f.old, &f.new, &without_markers),
        without_annotation_lines(hunks_only)
    );
}

/// `patch` without Git's `\ No newline at end of file` annotation lines (whole lines only).
fn without_annotation_lines(patch: &str) -> String {
    patch
        .split_inclusive('\n')
        .filter(|line| *line != "\\ No newline at end of file\n")
        .collect()
}

#[test]
fn source_lines_reading_like_the_marker_survive_omitted_markers() {
    let marker = "\\ No newline at end of file";
    let cases = [
        (
            "context",
            format!("{marker}\nold\n"),
            format!("{marker}\nnew\n"),
        ),
        ("added", "a\n".to_owned(), format!("a\n{marker}\n")),
        ("deleted", format!("a\n{marker}\n"), "a\n".to_owned()),
        (
            "with real marker",
            format!("{marker}\nx"),
            format!("{marker}\ny"),
        ),
    ];
    let omit = UnifiedDiffOptions::new().with_no_newline_marker(NoNewlineMarker::Omit);
    for (case, old, new) in cases {
        assert_matches_git(case, &old, &new, &[], &UnifiedDiffOptions::new());
        let git = from_label_lines(&git_no_index(&old, &new, ("old.txt", "new.txt"), &[]));
        let expected = without_annotation_lines(&git);
        let grit = unified_diff(&old, &new, &omit.clone().with_paths("old.txt", "new.txt"));
        assert_eq!(grit, expected, "{case}");
        assert!(
            grit.lines()
                .any(|line| line.ends_with(marker) && line.len() == marker.len() + 1),
            "{case}: the source line must stay with its prefix:\n{grit}"
        );
    }
}

#[test]
fn identical_and_whitespace_equal_sides_render_nothing_like_git() {
    let options = UnifiedDiffOptions::new();
    assert_matches_git("identical", C_OLD, C_OLD, &[], &options);
    let ws = UnifiedDiffOptions::new().with_whitespace(WhitespaceMode::ignore_all_space());
    assert_matches_git("only spaces", "a b c\n", "abc\n", &["-w"], &ws);
}
