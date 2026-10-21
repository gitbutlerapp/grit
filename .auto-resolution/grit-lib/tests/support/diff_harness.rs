//! Hermetic git/grit diff test helpers: blob patch oracles and tree/index/worktree raw matrices.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::diff::diffstat::{write_diffstat_block, DiffstatOptions, FileStatInput};
use grit_lib::diff::merge_file;
use grit_lib::diff::ws::ws_blank_line;
use grit_lib::diff::{
    anchored_unified_diff, count_changes_with_algorithm, count_git_lines, diff_commits,
    diff_index_to_tree, diff_index_to_worktree, diff_tree_to_worktree, diff_trees,
    diff_trees_show_tree_entries, format_raw, format_raw_abbrev, indent_heuristic_from_config,
    normalize_ignore_space_change, postprocess_diff_entries, tree_oid_of_commit,
    unified_diff_histogram_ignore_with_prefix_and_funcname,
    unified_diff_with_prefix_and_funcname_and_algorithm, DiffEntry, DiffPostprocessOptions,
    DiffPostprocessWorktree,
};
use grit_lib::index::Index;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
use similar::Algorithm;

use super::history_harness::git;
use super::refs_harness::null_device_for_tests;

/// Line diff algorithm selection matching Git's `--diff-algorithm` / `--histogram` / etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineDiffAlgo {
    Myers,
    Histogram,
    Patience,
    Minimal,
}

impl LineDiffAlgo {
    fn similar_algorithm(self) -> Algorithm {
        match self {
            LineDiffAlgo::Myers => Algorithm::Myers,
            LineDiffAlgo::Histogram => Algorithm::Myers,
            LineDiffAlgo::Patience => Algorithm::Patience,
            LineDiffAlgo::Minimal => Algorithm::Lcs,
        }
    }

    fn use_git_histogram(self) -> bool {
        matches!(self, LineDiffAlgo::Histogram)
    }

    fn git_extra_args(self) -> Vec<&'static str> {
        match self {
            LineDiffAlgo::Myers => vec![],
            LineDiffAlgo::Histogram => vec!["--histogram"],
            LineDiffAlgo::Patience => vec!["--patience"],
            LineDiffAlgo::Minimal => vec!["--minimal"],
        }
    }
}

/// Git whitespace flags for blob diffs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhitespaceMode {
    #[default]
    None,
    IgnoreSpaceChange,
    IgnoreAllSpace,
    IgnoreSpaceAtEol,
    IgnoreBlankLines,
    IgnoreCrAtEol,
}

impl WhitespaceMode {
    fn git_args(self) -> Vec<&'static str> {
        match self {
            WhitespaceMode::None => vec![],
            WhitespaceMode::IgnoreSpaceChange => vec!["-b"],
            WhitespaceMode::IgnoreAllSpace => vec!["-w"],
            WhitespaceMode::IgnoreSpaceAtEol => vec!["--ignore-space-at-eol"],
            WhitespaceMode::IgnoreBlankLines => vec!["--ignore-blank-lines"],
            WhitespaceMode::IgnoreCrAtEol => vec!["--ignore-cr-at-eol"],
        }
    }
}

/// Options for [`grit_blob_patch`].
#[derive(Debug, Clone)]
pub struct GritBlobPatchOpts {
    pub algo: LineDiffAlgo,
    pub ws: WhitespaceMode,
    pub context_lines: usize,
    pub inter_hunk_context: usize,
    pub indent_heuristic: bool,
    pub function_context: bool,
    pub anchors: Vec<String>,
    pub use_anchored: bool,
}

impl Default for GritBlobPatchOpts {
    fn default() -> Self {
        Self {
            algo: LineDiffAlgo::Myers,
            ws: WhitespaceMode::None,
            context_lines: 3,
            inter_hunk_context: 0,
            indent_heuristic: true,
            function_context: false,
            anchors: Vec::new(),
            use_anchored: false,
        }
    }
}

/// One old/new blob pair with a display name (upstream `t/` scenario label).
#[derive(Debug, Clone)]
pub struct BlobPair {
    pub name: &'static str,
    pub old: &'static [u8],
    pub new: &'static [u8],
}

struct TempPair {
    _dir: tempfile::TempDir,
    old_path: PathBuf,
    new_path: PathBuf,
}

fn write_temp_pair(old: &[u8], new: &[u8], label: &str) -> TempPair {
    let dir = tempfile::tempdir().expect("tempdir");
    let safe: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let old_path = dir.path().join(format!("{safe}.old"));
    let new_path = dir.path().join(format!("{safe}.new"));
    fs::write(&old_path, old).expect("write old");
    fs::write(&new_path, new).expect("write new");
    TempPair {
        _dir: dir,
        old_path,
        new_path,
    }
}

fn hermetic_git_diff(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", null_device_for_tests())
        .env("LC_ALL", "C")
        .output()
        .expect("spawn git")
}

/// Run `git diff --no-index` on two byte blobs and return stdout as UTF-8 lossy string.
pub fn git_diff_no_index_patch(
    old: &[u8],
    new: &[u8],
    path: &str,
    algo: LineDiffAlgo,
    ws: WhitespaceMode,
    extra_git_args: &[&str],
) -> String {
    let tmp = write_temp_pair(old, new, path);
    let old_s = tmp.old_path.to_string_lossy();
    let new_s = tmp.new_path.to_string_lossy();
    let mut args: Vec<String> = vec![
        "diff".into(),
        "--no-index".into(),
        "--no-color".into(),
        format!("-U{}", 3),
    ];
    for a in algo.git_extra_args() {
        args.push(a.into());
    }
    for a in ws.git_args() {
        args.push(a.into());
    }
    for a in extra_git_args {
        args.push((*a).into());
    }
    args.push("--".into());
    args.push(old_s.into_owned());
    args.push(new_s.into_owned());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = hermetic_git_diff(tmp._dir.path(), &arg_refs);
    assert!(
        out.status.code() == Some(1) || out.status.success(),
        "git diff failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn strip_trailing_cr_line(s: &str) -> String {
    if s.ends_with('\r') {
        s[..s.len().saturating_sub(1)].to_string()
    } else {
        s.to_string()
    }
}

fn trim_trailing_ws_line(s: &str) -> String {
    s.trim_end_matches(|c: char| c.is_whitespace()).to_string()
}

fn strip_all_whitespace_line(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn preprocess_for_whitespace(old: &str, new: &str, ws: WhitespaceMode) -> (String, String) {
    match ws {
        WhitespaceMode::None | WhitespaceMode::IgnoreBlankLines => {
            (old.to_string(), new.to_string())
        }
        WhitespaceMode::IgnoreSpaceChange => (
            normalize_ignore_space_change(old),
            normalize_ignore_space_change(new),
        ),
        WhitespaceMode::IgnoreAllSpace => (
            old.lines()
                .map(strip_all_whitespace_line)
                .collect::<Vec<_>>()
                .join("\n"),
            new.lines()
                .map(strip_all_whitespace_line)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        WhitespaceMode::IgnoreSpaceAtEol => (
            old.lines()
                .map(trim_trailing_ws_line)
                .collect::<Vec<_>>()
                .join("\n"),
            new.lines()
                .map(trim_trailing_ws_line)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        WhitespaceMode::IgnoreCrAtEol => (
            old.lines()
                .map(strip_trailing_cr_line)
                .collect::<Vec<_>>()
                .join("\n"),
            new.lines()
                .map(strip_trailing_cr_line)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    }
}

fn text_or_lossy(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Build a unified diff patch for two blobs using grit-lib diff APIs.
pub fn grit_blob_patch(old: &[u8], new: &[u8], path: &str, opts: &GritBlobPatchOpts) -> String {
    if merge_file::is_binary(old) || merge_file::is_binary(new) {
        let old_path = format!("a/{path}");
        let new_path = format!("b/{path}");
        let body = grit_lib::apply::write_git_binary_patch(old, new, &old_path, &new_path)
            .expect("binary patch");
        return format!("--- {old_path}\n+++ {new_path}\n{body}");
    }

    let old_t = text_or_lossy(old);
    let new_t = text_or_lossy(new);
    let (old_in, new_in) = preprocess_for_whitespace(&old_t, &new_t, opts.ws);

    let old_path = format!("a/{path}");
    let new_path = format!("b/{path}");
    let algo = opts.algo.similar_algorithm();
    let histogram = opts.algo.use_git_histogram();

    if opts.use_anchored {
        return grit_lib::diff::annotate_unified_diff_no_newline_at_eof(
            &anchored_unified_diff(
                &old_in,
                &new_in,
                &old_path,
                &new_path,
                opts.context_lines,
                &opts.anchors,
                algo,
                histogram,
                opts.indent_heuristic,
                false,
            ),
            &old_t,
            &new_t,
        );
    }

    if matches!(opts.ws, WhitespaceMode::IgnoreBlankLines) && histogram {
        return grit_lib::diff::annotate_unified_diff_no_newline_at_eof(
            &unified_diff_histogram_ignore_with_prefix_and_funcname(
                &old_in,
                &new_in,
                &old_path,
                &new_path,
                opts.context_lines,
                opts.inter_hunk_context,
                "",
                "",
                None,
                false,
                false,
                opts.indent_heuristic,
                |removed, added| {
                    removed.iter().all(|l| ws_blank_line(l))
                        && added.iter().all(|l| ws_blank_line(l))
                },
            ),
            &old_t,
            &new_t,
        );
    }

    grit_lib::diff::annotate_unified_diff_no_newline_at_eof(
        &unified_diff_with_prefix_and_funcname_and_algorithm(
            &old_in,
            &new_in,
            &old_path,
            &new_path,
            opts.context_lines,
            opts.inter_hunk_context,
            "",
            "",
            None,
            algo,
            opts.function_context,
            histogram,
            opts.indent_heuristic,
            false,
        ),
        &old_t,
        &new_t,
    )
}

/// Strip a patch to comparable hunk bodies (from first `@@` through end), normalizing line endings.
pub fn patch_hunk_body(patch: &str) -> String {
    let normalized = patch.replace("\r\n", "\n");
    let mut lines = normalized.lines();
    while let Some(line) = lines.next() {
        if line.starts_with("@@ ") {
            let mut out = String::from(line);
            out.push('\n');
            for rest in lines {
                if rest.starts_with("\\ No newline at end of file") {
                    continue;
                }
                out.push_str(rest);
                out.push('\n');
            }
            while out.ends_with("\n\n") {
                out.pop();
            }
            if !out.ends_with('\n') {
                out.push('\n');
            }
            return out;
        }
    }
    String::new()
}

fn strip_index_lines(patch: &str) -> String {
    patch
        .lines()
        .filter(|l| !l.starts_with("index ") && !l.starts_with("diff --git"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
        + "\n"
}

/// Assert grit patch matches git oracle (hunk bodies by default; full patch when `compare_full`).
pub fn assert_patch_matches_git(
    old: &[u8],
    new: &[u8],
    path: &str,
    opts: &GritBlobPatchOpts,
    extra_git_args: &[&str],
    compare_full: bool,
) {
    let git_patch = git_diff_no_index_patch(old, new, path, opts.algo, opts.ws, extra_git_args);
    let grit_patch = grit_blob_patch(old, new, path, opts);
    if compare_full {
        let g = strip_index_lines(&git_patch);
        let r = strip_index_lines(&grit_patch);
        assert_eq!(g, r, "full patch mismatch for {}", path);
        return;
    }
    let g = patch_hunk_body(&git_patch);
    let r = patch_hunk_body(&grit_patch);
    assert_eq!(g, r, "hunk body mismatch for {}", path);
}

/// Run `git apply --check` on `patch` with `old_bytes` as the starting file at `path`.
pub fn git_apply_check(old_bytes: &[u8], patch: &str, path: &str) -> bool {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join(path);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(&file, old_bytes).expect("write base");
    let patch_file = dir.path().join("patch.diff");
    fs::write(&patch_file, patch).expect("write patch");
    let check = hermetic_git_diff(
        dir.path(),
        &["apply", "--check", patch_file.to_str().expect("utf8")],
    );
    check.status.success()
}

/// Apply patch and compare file contents to expected `new_bytes`.
pub fn git_apply_and_compare(old_bytes: &[u8], new_bytes: &[u8], patch: &str, path: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join(path);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(&file, old_bytes).expect("write base");
    let patch_file = dir.path().join("patch.diff");
    fs::write(&patch_file, patch).expect("write patch");
    let check = hermetic_git_diff(
        dir.path(),
        &["apply", "--check", patch_file.to_str().expect("utf8")],
    );
    assert!(
        check.status.success(),
        "git apply --check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let apply = hermetic_git_diff(dir.path(), &["apply", patch_file.to_str().expect("utf8")]);
    assert!(
        apply.status.success(),
        "git apply failed: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let got = fs::read(&file).expect("read result");
    assert_eq!(got, new_bytes, "applied content mismatch");
}

pub fn config_from_snippet(text: &str) -> ConfigSet {
    let path = Path::new(".git/config");
    let file = ConfigFile::parse(path, text, ConfigScope::Local).expect("parse config");
    let mut set = ConfigSet::new();
    set.merge(&file);
    set
}

pub fn resolve_indent(config: &ConfigSet, cli_on: bool, cli_off: bool) -> bool {
    grit_lib::diff::resolve_indent_heuristic(config, cli_on, cli_off)
}

pub fn indent_from_config(config: &ConfigSet) -> bool {
    indent_heuristic_from_config(config)
}

/// Ported blob pairs from upstream `t4033`, `t4050`, `t4071`, `t4065`, `t4061`, `t4015`, etc.
pub fn blob_pairs() -> Vec<BlobPair> {
    let mut v = Vec::new();
    v.extend(mixed_layout_pair());
    v.extend(unique_pairs());
    v.extend(patience_eol_pair());
    v.extend(minimal_pair());
    v.extend(anchored_pairs());
    v.extend(indent_pairs());
    v.extend(whitespace_pairs());
    v.extend(histogram_shift_pairs());
    v.extend(inter_hunk_pairs());
    v.extend(no_newline_pairs());
    v.extend(binary_pairs());
    v.extend(pathological_pairs());
    v
}

fn mixed_layout_pair() -> Vec<BlobPair> {
    vec![BlobPair {
        name: "t4033/t4050-mixed-layout",
        old: include_bytes!("data/diff_frobnitz_old.c"),
        new: include_bytes!("data/diff_frobnitz_new.c"),
    }]
}

fn unique_pairs() -> Vec<BlobPair> {
    vec![
        BlobPair {
            name: "t4033/t4050-unique",
            old: b"1\n2\n3\n4\n5\n6\n",
            new: b"a\nb\nc\nd\ne\nf\n",
        },
        BlobPair {
            name: "t4071-minimal",
            old: b"x\nx\nx\nx\n",
            new: b"x\nx\nx\nA\nB\nC\nD\nx\nE\nF\nG\n",
        },
    ]
}

fn patience_eol_pair() -> Vec<BlobPair> {
    vec![BlobPair {
        name: "t4033-ignore-space-at-eol",
        old: b"a\nb\nc\n",
        new: b"a\nbX\nc\n",
    }]
}

fn minimal_pair() -> Vec<BlobPair> {
    vec![BlobPair {
        name: "t4071-minimal-lines",
        old: b"x\nx\nx\nx\n",
        new: b"x\nx\nx\nA\nB\nC\nD\nx\nE\nF\nG\n",
    }]
}

fn anchored_pairs() -> Vec<BlobPair> {
    vec![
        BlobPair {
            name: "t4065-rotate-abc",
            old: b"a\nb\nc\n",
            new: b"c\na\nb\n",
        },
        BlobPair {
            name: "t4065-multi-anchor",
            old: b"a\nb\nc\nd\ne\nf\n",
            new: b"c\na\nb\nf\nd\ne\n",
        },
    ]
}

fn indent_pairs() -> Vec<BlobPair> {
    vec![
        BlobPair {
            name: "t4061-spaces",
            old: b"1\n2\na\n \nb\n3\n4\n",
            new: b"1\n2\na\n \nb\na\n\nb\n3\n4\n",
        },
        BlobPair {
            name: "t4061-functions",
            old: b"1\n2\n/* function */\nfoo() {\n    foo\n}\n\n3\n4\n",
            new: b"1\n2\n/* function */\nbar() {\n    foo\n}\n\n/* function */\nfoo() {\n    foo\n}\n\n3\n4\n",
        },
    ]
}

fn whitespace_pairs() -> Vec<BlobPair> {
    vec![
        BlobPair {
            name: "t4015-space-change",
            old: b"foo\n",
            new: b"f o o\n",
        },
        BlobPair {
            name: "t4015-trailing-ws",
            old: b"line\t\n",
            new: b"line  \n",
        },
        BlobPair {
            name: "t4029-trailing-space",
            old: b"unchanged\nspace   \n",
            new: b"unchanged\nspace\n",
        },
    ]
}

fn histogram_shift_pairs() -> Vec<BlobPair> {
    vec![
        BlobPair {
            name: "t4074-shifted-group",
            old: b"A\nx\nA\nA\nA\nx\nA\nA\nA\n",
            new: b"A\nx\nA\nZ\nA\nx\nA\nA\nA\n",
        },
        BlobPair {
            name: "t4074-merged-no-shift",
            old: b"A\nZ\nB\nx\n",
            new: b"C\nD\nx\nZ\nE\nx\n",
        },
    ]
}

fn inter_hunk_pairs() -> Vec<BlobPair> {
    let f1_old = b"A\n1\nB\n";
    let f1_new = b"X\n1\nY\n";
    vec![BlobPair {
        name: "t4032-f1-u0",
        old: f1_old,
        new: f1_new,
    }]
}

fn no_newline_pairs() -> Vec<BlobPair> {
    vec![
        BlobPair {
            name: "no-newline-at-eof-old",
            old: b"one",
            new: b"one\n",
        },
        BlobPair {
            name: "no-newline-at-eof-both",
            old: b"alpha",
            new: b"beta",
        },
    ]
}

fn binary_pairs() -> Vec<BlobPair> {
    vec![
        BlobPair {
            name: "t4012-text-tweak",
            old: b"AIT\n",
            new: b"git\n",
        },
        BlobPair {
            name: "t4012-binary",
            old: b"\0BIN\0",
            new: b"\0BIN2\0",
        },
    ]
}

fn pathological_pairs() -> Vec<BlobPair> {
    vec![
        BlobPair {
            name: "pathological-long-common",
            old: b"a\na\na\na\na\nb\n",
            new: b"a\na\na\na\na\nc\n",
        },
        BlobPair {
            name: "pathological-many-dups",
            old: b"x\nx\nx\ny\n",
            new: b"x\nx\ny\ny\n",
        },
    ]
}

pub fn grit_numstat(old: &str, new: &str, algo: LineDiffAlgo) -> (usize, usize) {
    count_changes_with_algorithm(old, new, algo.similar_algorithm(), algo.use_git_histogram())
}

pub fn git_numstat(old: &[u8], new: &[u8], path: &str, algo: LineDiffAlgo) -> (usize, usize) {
    let patch = git_diff_no_index_patch(old, new, path, algo, WhitespaceMode::None, &["--numstat"]);
    let line = patch.lines().next().unwrap_or("");
    let mut parts = line.split('\t');
    let ins = parts.next().unwrap_or("-");
    let del = parts.next().unwrap_or("-");
    let parse = |s: &str| {
        if s == "-" {
            0
        } else {
            s.parse().unwrap_or(0)
        }
    };
    (parse(ins), parse(del))
}

pub fn grit_stat_line(path: &str, ins: usize, del: usize, width: usize) -> String {
    grit_lib::diff::format_stat_line_width(path, ins, del, width, 0)
}

pub fn git_stat_width_line(old: &[u8], new: &[u8], path: &str, width: usize) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join(path);
    if let Some(parent) = file.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).expect("mkdir");
        }
    }
    fs::write(&file, old).expect("write old");
    let init = hermetic_git_diff(dir.path(), &["init"]);
    assert!(init.status.success(), "git init failed");
    let config = |k: &str, v: &str| {
        let out = hermetic_git_diff(dir.path(), &["config", k, v]);
        assert!(out.status.success(), "git config {k} failed");
    };
    config("user.email", "grit-test@example.com");
    config("user.name", "grit-test");
    let add = hermetic_git_diff(dir.path(), &["add", "-A"]);
    assert!(add.status.success(), "git add failed");
    let commit = hermetic_git_diff(dir.path(), &["commit", "-m", "base"]);
    assert!(commit.status.success(), "git commit failed");
    fs::write(&file, new).expect("write new");
    let stat_arg = format!("--stat-width={width}");
    let diff = hermetic_git_diff(dir.path(), &["diff", "--no-color", &stat_arg]);
    assert!(
        diff.status.success() || diff.status.code() == Some(0),
        "git diff --stat failed: {}",
        String::from_utf8_lossy(&diff.stderr)
    );
    String::from_utf8_lossy(&diff.stdout)
        .lines()
        .find(|l| l.contains(" | "))
        .map(|l| l.to_string())
        .unwrap_or_default()
}

pub fn grit_diffstat_block(path: &str, old: &[u8], new: &[u8], algo: LineDiffAlgo) -> String {
    let old_s = text_or_lossy(old);
    let new_s = text_or_lossy(new);
    let (ins, del) = grit_numstat(&old_s, &new_s, algo);
    let mut buf = Vec::new();
    let files = [FileStatInput {
        path_display: path.to_string(),
        insertions: ins,
        deletions: del,
        is_binary: merge_file::is_binary(old) || merge_file::is_binary(new),
        is_unmerged: false,
    }];
    let opts = DiffstatOptions {
        terminal_width: 80,
        total_width: 80,
        stat_name_width: None,
        stat_graph_width: None,
        stat_count: None,
        line_prefix: "",
        width_prefix: "",
        subtract_prefix_from_terminal: false,
        graph_prefix_budget_slack: 0,
        color_add: "",
        color_del: "",
        color_reset: "",
        graph_bar_slack: 0,
    };
    write_diffstat_block(&mut buf, &files, &opts).expect("stat block");
    String::from_utf8(buf).expect("utf8 stat")
}

pub fn git_count_lines(data: &[u8]) -> usize {
    count_git_lines(data)
}

pub fn git_quiet(dir: &Path, args: &[&str]) -> String {
    git(dir, args)
}

pub fn git_output(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_device_for_tests())
        .env("GIT_CONFIG_SYSTEM", null_device_for_tests())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Diff Test")
        .env("GIT_AUTHOR_EMAIL", "diff@example.com")
        .env("GIT_COMMITTER_NAME", "Diff Test")
        .env("GIT_COMMITTER_EMAIL", "diff@example.com")
        .output()
        .expect("spawn git")
}

pub fn rev_parse_tree(dir: &Path, rev: &str) -> ObjectId {
    git_quiet(dir, &["rev-parse", rev])
        .trim()
        .parse()
        .expect("tree oid")
}

pub fn config_with_kv(section: &str, key: &str, value: &str) -> ConfigSet {
    let content = format!("[{section}]\n\t{key} = {value}\n");
    let file = ConfigFile::parse(
        std::path::Path::new("test.cfg"),
        &content,
        ConfigScope::Local,
    )
    .expect("parse config");
    let mut set = ConfigSet::new();
    set.merge(&file);
    set
}

pub fn grit_raw_lines(entries: &[DiffEntry]) -> Vec<String> {
    let mut lines: Vec<String> = entries.iter().map(format_raw).collect();
    lines.sort();
    lines
}

pub fn grit_raw_lines_abbrev(entries: &[DiffEntry], abbrev: usize) -> Vec<String> {
    let mut lines: Vec<String> = entries
        .iter()
        .map(|e| format_raw_abbrev(e, abbrev))
        .collect();
    lines.sort();
    lines
}

pub fn git_raw_lines(dir: &Path, args: &[&str]) -> Vec<String> {
    let out = git_output(dir, args);
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with(':'))
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

pub fn assert_raw_matches_git(grit_lines: &[String], git_lines: &[String], context: &str) {
    assert_eq!(
        git_lines,
        grit_lines,
        "{context}: grit raw diff must match system git\n\
         git:\n{}\ngrit:\n{}",
        git_lines.join("\n"),
        grit_lines.join("\n")
    );
}

pub fn postprocess(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    source_tree: Option<&ObjectId>,
    work_root: Option<&Path>,
    worktree: Option<&DiffPostprocessWorktree<'_>>,
    config: &ConfigSet,
    opts: &DiffPostprocessOptions,
) -> Vec<DiffEntry> {
    postprocess_diff_entries(
        odb,
        entries,
        source_tree,
        work_root,
        worktree,
        config,
        &ThreadPolicy::default(),
        opts,
    )
    .expect("postprocess")
}

pub struct DiffRepo {
    _guard: tempfile::TempDir,
    pub worktree: PathBuf,
    pub repo: Repository,
}

impl DiffRepo {
    pub fn init() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let worktree = root.path().to_path_buf();
        git_quiet(&worktree, &["init", "-q", "-b", "main"]);
        let repo = Repository::discover(Some(worktree.as_path()), &RepositoryOptions::empty())
            .expect("open");
        Self {
            _guard: root,
            worktree,
            repo,
        }
    }

    pub fn load_index(&self) -> Index {
        self.repo.index_handle().load().expect("index")
    }
}

pub fn tree_diff_raw(
    repo: &Repository,
    old_tree: Option<&ObjectId>,
    new_tree: Option<&ObjectId>,
    config: &ConfigSet,
    opts: &DiffPostprocessOptions,
) -> Vec<String> {
    let raw = diff_trees(repo.odb(), old_tree, new_tree, "").expect("diff_trees");
    let entries = postprocess(repo.odb(), raw, old_tree, None, None, config, opts);
    grit_raw_lines(&entries)
}

pub fn index_to_tree_raw(
    repo: &Repository,
    index: &Index,
    tree: &ObjectId,
    config: &ConfigSet,
    opts: &DiffPostprocessOptions,
) -> Vec<String> {
    let raw = diff_index_to_tree(repo.odb(), index, Some(tree), false).expect("diff_index_to_tree");
    let entries = postprocess(repo.odb(), raw, Some(tree), None, None, config, opts);
    grit_raw_lines(&entries)
}

pub fn index_to_worktree_raw(
    repo: &Repository,
    worktree: &Path,
    index: &mut Index,
    head_tree: Option<&ObjectId>,
    config: &ConfigSet,
    opts: &DiffPostprocessOptions,
) -> Vec<String> {
    let raw = diff_index_to_worktree(repo.odb(), index, worktree, false, false)
        .expect("diff_index_to_worktree");
    let entries = postprocess(
        repo.odb(),
        raw,
        head_tree,
        Some(worktree),
        None,
        config,
        opts,
    );
    grit_raw_lines(&entries)
}

fn tree_worktree_side<'a>(repo: &'a Repository, index: &'a Index) -> DiffPostprocessWorktree<'a> {
    DiffPostprocessWorktree {
        repository_git_dir: repo.git_dir(),
        index,
    }
}

pub fn tree_to_worktree_raw(
    repo: &Repository,
    worktree: &Path,
    index: &Index,
    tree: Option<&ObjectId>,
    config: &ConfigSet,
    opts: &DiffPostprocessOptions,
) -> Vec<String> {
    let raw =
        diff_tree_to_worktree(repo.odb(), tree, worktree, index).expect("diff_tree_to_worktree");
    let wctx = tree_worktree_side(repo, index);
    let entries = postprocess(
        repo.odb(),
        raw,
        tree,
        Some(worktree),
        Some(&wctx),
        config,
        opts,
    );
    grit_raw_lines(&entries)
}

/// Like [`tree_to_worktree_raw`], but abbreviates OIDs like `git diff <tree> --raw`.
pub fn tree_to_worktree_raw_abbrev(
    repo: &Repository,
    worktree: &Path,
    index: &Index,
    tree: Option<&ObjectId>,
    config: &ConfigSet,
    opts: &DiffPostprocessOptions,
    abbrev: usize,
) -> Vec<String> {
    let raw =
        diff_tree_to_worktree(repo.odb(), tree, worktree, index).expect("diff_tree_to_worktree");
    let wctx = tree_worktree_side(repo, index);
    let entries = postprocess(
        repo.odb(),
        raw,
        tree,
        Some(worktree),
        Some(&wctx),
        config,
        opts,
    );
    grit_raw_lines_abbrev(&entries, abbrev)
}

pub fn commits_diff_raw(
    repo: &Repository,
    base: &ObjectId,
    tip: &ObjectId,
    config: &ConfigSet,
    opts: &DiffPostprocessOptions,
) -> Vec<String> {
    let raw = diff_commits(repo.odb(), base, tip).expect("diff_commits");
    let base_tree = tree_oid_of_commit(repo.odb(), base).expect("base tree");
    let entries = postprocess(repo.odb(), raw, Some(&base_tree), None, None, config, opts);
    grit_raw_lines(&entries)
}

pub fn head_tree_oid(repo: &Repository) -> Option<ObjectId> {
    let head = resolve_head(repo.git_dir()).ok()?;
    let commit = head.oid()?;
    tree_oid_of_commit(repo.odb(), commit).ok()
}

/// Write a malformed tree object into `odb` and return its oid.
pub fn write_bogus_tree(odb: &Odb) -> ObjectId {
    odb.write(ObjectKind::Tree, b"not a valid tree encoding")
        .expect("write bogus tree")
}
