//! Hermetic git/grit diff test helpers: blob patch oracles and tree/index/worktree raw matrices.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::diff::merge_file;
use grit_lib::diff::{
    count_git_lines, format_raw, format_raw_abbrev, tree_oid_of_commit, unified_diff,
    DiffAlgorithm, DiffEntry, HunkExtent, IndentHeuristic, TreeDiffOptions, UnifiedDiffOptions,
};
use grit_lib::index::Index;
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;

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
    fn diff_algorithm(self) -> DiffAlgorithm {
        match self {
            LineDiffAlgo::Myers => DiffAlgorithm::Myers,
            LineDiffAlgo::Histogram => DiffAlgorithm::Histogram,
            LineDiffAlgo::Patience => DiffAlgorithm::Patience,
            LineDiffAlgo::Minimal => DiffAlgorithm::Minimal,
        }
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
    /// The grit-lib comparison rules for this mode (`--ignore-cr-at-eol` has none; the harness
    /// strips the carriage returns from the input instead).
    fn library_mode(self) -> grit_lib::diff::WhitespaceMode {
        let mut mode = grit_lib::diff::WhitespaceMode::default();
        match self {
            WhitespaceMode::None | WhitespaceMode::IgnoreCrAtEol => {}
            WhitespaceMode::IgnoreSpaceChange => mode.ignore_space_change = true,
            WhitespaceMode::IgnoreAllSpace => mode.ignore_all_space = true,
            WhitespaceMode::IgnoreSpaceAtEol => mode.ignore_space_at_eol = true,
            WhitespaceMode::IgnoreBlankLines => mode.ignore_blank_lines = true,
        }
        mode
    }

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

fn strip_cr_at_eol(text: &str) -> String {
    text.lines()
        .map(strip_trailing_cr_line)
        .collect::<Vec<_>>()
        .join("\n")
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
    let (old_in, new_in) = if opts.ws == WhitespaceMode::IgnoreCrAtEol {
        (strip_cr_at_eol(&old_t), strip_cr_at_eol(&new_t))
    } else {
        (old_t, new_t)
    };

    let mut options = UnifiedDiffOptions::new()
        .with_paths(path, path)
        .with_algorithm(opts.algo.diff_algorithm())
        .with_context_lines(opts.context_lines)
        .with_inter_hunk_context(opts.inter_hunk_context)
        .with_indent_heuristic(if opts.indent_heuristic {
            IndentHeuristic::Enabled
        } else {
            IndentHeuristic::Disabled
        })
        .with_extent(if opts.function_context {
            HunkExtent::Function
        } else {
            HunkExtent::Context
        })
        .with_whitespace(opts.ws.library_mode());
    if opts.use_anchored {
        options = options.with_anchors(opts.anchors.iter().cloned());
    }
    unified_diff(&old_in, &new_in, &options)
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
    grit_lib::diff::DiffOptions::resolve_indent_heuristic(config, cli_on, cli_off)
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
    let options = UnifiedDiffOptions::new().with_algorithm(algo.diff_algorithm());
    let stat = grit_lib::diff::line_change_counts(old, new, &options);
    (stat.insertions, stat.deletions)
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
        self.repo
            .index()
            .load(&grit_lib::index::IndexLoadOptions::default())
            .expect("index")
    }
}

pub fn tree_diff_raw(
    repo: &Repository,
    old_tree: Option<&ObjectId>,
    new_tree: Option<&ObjectId>,
    opts: &TreeDiffOptions,
) -> Vec<String> {
    let result = repo
        .diff()
        .tree_to_tree(old_tree, new_tree, opts)
        .expect("tree_to_tree");
    grit_raw_lines(&result.entries)
}

pub fn index_to_tree_raw(
    repo: &Repository,
    index: &Index,
    tree: &ObjectId,
    opts: &TreeDiffOptions,
) -> Vec<String> {
    let result = repo
        .diff()
        .index_to_tree(index, Some(tree), opts)
        .expect("index_to_tree");
    grit_raw_lines(&result.entries)
}

pub fn index_to_worktree_raw(
    repo: &Repository,
    index: &mut Index,
    opts: &TreeDiffOptions,
) -> Vec<String> {
    let result = repo
        .diff()
        .index_to_worktree(index, opts)
        .expect("index_to_worktree");
    grit_raw_lines(&result.entries)
}

pub fn tree_to_worktree_raw(
    repo: &Repository,
    index: &Index,
    tree: Option<&ObjectId>,
    opts: &TreeDiffOptions,
) -> Vec<String> {
    let result = repo
        .diff()
        .tree_to_worktree(tree, index, opts)
        .expect("tree_to_worktree");
    grit_raw_lines(&result.entries)
}

/// Like [`tree_to_worktree_raw`], but abbreviates OIDs like `git diff <tree> --raw`.
pub fn tree_to_worktree_raw_abbrev(
    repo: &Repository,
    index: &Index,
    tree: Option<&ObjectId>,
    opts: &TreeDiffOptions,
    abbrev: usize,
) -> Vec<String> {
    let result = repo
        .diff()
        .tree_to_worktree(tree, index, opts)
        .expect("tree_to_worktree");
    grit_raw_lines_abbrev(&result.entries, abbrev)
}

pub fn commits_diff_raw(
    repo: &Repository,
    base: &ObjectId,
    tip: &ObjectId,
    opts: &TreeDiffOptions,
) -> Vec<String> {
    let result = repo.diff().commits(base, tip, opts).expect("commits");
    grit_raw_lines(&result.entries)
}

/// Set `key` to `value` in the repository's local config through the system `git`.
pub fn set_repo_config(worktree: &Path, key: &str, value: &str) {
    git_quiet(worktree, &["config", key, value]);
}

pub fn head_tree_oid(repo: &Repository) -> Option<ObjectId> {
    let head = repo.references().head().ok()?;
    let commit = head.oid()?;
    tree_oid_of_commit(repo.odb(), commit).ok()
}

/// Write a malformed tree object into `odb` and return its oid.
pub fn write_bogus_tree(odb: &Odb) -> ObjectId {
    odb.write(ObjectKind::Tree, b"not a valid tree encoding")
        .expect("write bogus tree")
}
