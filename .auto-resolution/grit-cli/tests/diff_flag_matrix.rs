//! Representative matrix: diff/show/log format and whitespace flags vs system `git`.
//!
//! One fixture covers renames, a binary file, whitespace-only edits, CRLF, and mode
//! changes. Each command mode runs a small set of flag combinations (each listed flag
//! appears at least once across the matrix).

use grit_test_support::{assert_eq_nice, git, git_cmd, grit, grit_cmd, write_file};
use serde_json::Value;
use std::path::{Path, PathBuf};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// First commit (base tree).
    c1: String,
    /// Rename, binary, CRLF, and mode change.
    c2: String,
    /// Small follow-up commit (tip).
    #[allow(dead_code)]
    c3: String,
}

impl Fixture {
    fn path(&self) -> &Path {
        &self.root
    }
}

fn setup_fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    write_file(
        &root,
        ".gitattributes",
        "*.crlf text eol=crlf\n*.bin binary\n",
    );
    write_file(&root, "plain.txt", "alpha\nbeta\ngamma\n");
    write_file(&root, "rename-me.txt", "shared\ncontent\n");
    std::fs::write(root.join("data.bin"), [0u8, 1, 2, 3, 4]).expect("binary");
    write_file(&root, "line.crlf", "one\ntwo\n");
    write_file(&root, "ws.txt", "hello world\n");
    write_file(&root, "mode.sh", "#!/bin/sh\necho ok\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);
    let c1 = git(&root, &["rev-parse", "HEAD"]).trim().to_string();

    git(&root, &["mv", "rename-me.txt", "renamed.txt"]);
    write_file(&root, "renamed.txt", "shared\ncontent\nextra\n");
    write_file(&root, "plain.txt", "alpha\nBETA\ngamma\n");
    std::fs::write(root.join("data.bin"), [0u8, 1, 2, 3, 4, 5]).expect("binary2");
    write_file(&root, "line.crlf", "one\nTWO\n");
    git(&root, &["update-index", "--chmod=+x", "mode.sh"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "rename-and-more"]);
    let c2 = git(&root, &["rev-parse", "HEAD"]).trim().to_string();

    write_file(&root, "plain.txt", "alpha\nBETA\ngamma\ndelta\n");
    git(&root, &["add", "plain.txt"]);
    git(&root, &["commit", "-qm", "tip"]);
    let c3 = git(&root, &["rev-parse", "HEAD"]).trim().to_string();

    // Worktree: whitespace-only, unstaged text, staged new file, unstaged binary tweak.
    write_file(&root, "ws.txt", "hello  world\n");
    write_file(&root, "plain.txt", "alpha\nBETA\ngamma\ndelta\nedited\n");
    std::fs::write(root.join("data.bin"), [0u8, 1, 2, 3, 4, 5, 6]).expect("binary3");
    write_file(&root, "staged-only.txt", "staged\n");
    git(&root, &["add", "staged-only.txt"]);

    assert!(grit(&root, &["init"]).ok(), "grit init");

    Fixture {
        _dir: dir,
        root,
        c1,
        c2,
        c3,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Out {
    Numstat,
    NameStatus,
    Json,
}

#[derive(Clone, Copy)]
enum Mode {
    TwoCommit,
    Staged,
    Worktree,
    Show,
    Log,
}

struct Case {
    mode: Mode,
    out: Out,
    flags: &'static [&'static str],
    label: &'static str,
}

fn git_log_limit() -> &'static str {
    "3"
}

fn append_format(out: Out, args: &mut Vec<String>) {
    match out {
        Out::Numstat => args.push("--numstat".to_string()),
        Out::NameStatus => args.push("--name-status".to_string()),
        Out::Json => {}
    }
}

/// `grit log --json` needs a diff detail flag to populate per-commit `files`.
fn append_log_json_detail(flags: &[&str], args: &mut Vec<String>) {
    if flags.iter().any(|f| *f == "-M30") {
        args.push("--name-status".to_string());
    } else {
        args.push("-p".to_string());
    }
}

fn build_git_args(fx: &Fixture, mode: Mode, out: Out, flags: &[&str]) -> Vec<String> {
    let mut args = Vec::new();
    match mode {
        Mode::TwoCommit => {
            args.push("diff".to_string());
            args.extend(flags.iter().map(|s| (*s).to_string()));
            append_format(out, &mut args);
            args.push(fx.c1.clone());
            args.push(fx.c2.clone());
        }
        Mode::Staged => {
            args.push("diff".to_string());
            args.push("--staged".to_string());
            args.extend(flags.iter().map(|s| (*s).to_string()));
            append_format(out, &mut args);
        }
        Mode::Worktree => {
            args.push("diff".to_string());
            args.extend(flags.iter().map(|s| (*s).to_string()));
            append_format(out, &mut args);
        }
        Mode::Show => {
            args.push("show".to_string());
            args.extend(flags.iter().map(|s| (*s).to_string()));
            append_format(out, &mut args);
            args.push("--format=".to_string());
            args.push(fx.c2.clone());
        }
        Mode::Log => {
            args.push("log".to_string());
            args.extend(flags.iter().map(|s| (*s).to_string()));
            if matches!(out, Out::Json) {
                append_log_json_detail(flags, &mut args);
            } else {
                append_format(out, &mut args);
            }
            args.push(format!("-{}", git_log_limit()));
        }
    }
    args
}

fn build_grit_args(fx: &Fixture, mode: Mode, out: Out, flags: &[&str]) -> Vec<String> {
    let mut args = Vec::new();
    if matches!(out, Out::Json) {
        args.push("--json".to_string());
    }
    match mode {
        Mode::TwoCommit => {
            args.push("diff".to_string());
            args.extend(flags.iter().map(|s| (*s).to_string()));
            append_format(out, &mut args);
            args.push(fx.c1.clone());
            args.push(fx.c2.clone());
        }
        Mode::Staged => {
            args.push("diff".to_string());
            args.push("--staged".to_string());
            args.extend(flags.iter().map(|s| (*s).to_string()));
            append_format(out, &mut args);
        }
        Mode::Worktree => {
            args.push("diff".to_string());
            args.push("--unstaged".to_string());
            args.extend(flags.iter().map(|s| (*s).to_string()));
            append_format(out, &mut args);
        }
        Mode::Show => {
            args.push("show".to_string());
            args.extend(flags.iter().map(|s| (*s).to_string()));
            append_format(out, &mut args);
            args.push(fx.c2.clone());
        }
        Mode::Log => {
            args.push("log".to_string());
            args.extend(flags.iter().map(|s| (*s).to_string()));
            if matches!(out, Out::Json) {
                append_log_json_detail(flags, &mut args);
            } else {
                append_format(out, &mut args);
            }
            args.push("--limit".to_string());
            args.push(git_log_limit().to_string());
        }
    }
    args
}

fn exec_git(fx: &Fixture, args: Vec<String>) -> grit_test_support::Output {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git_cmd(&refs).in_dir(fx.path()).exec()
}

fn exec_grit(fx: &Fixture, args: Vec<String>) -> grit_test_support::Output {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    grit_cmd(&refs).in_dir(fx.path()).exec()
}

fn is_numstat_line(line: &str) -> bool {
    let mut tabs = line.split('\t');
    let a = tabs.next();
    let b = tabs.next();
    let path = tabs.next();
    matches!((a, b, path), (Some(a), Some(b), Some(p)) if !p.is_empty()
        && (a == "-" || a.chars().all(|c| c.is_ascii_digit()))
        && (b == "-" || b.chars().all(|c| c.is_ascii_digit())))
}

fn is_name_status_line(line: &str) -> bool {
    let Some((status, rest)) = line.split_once('\t') else {
        return false;
    };
    !status.is_empty()
        && status
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && !rest.is_empty()
}

fn sort_name_status_lines(text: &str) -> String {
    let mut lines: Vec<_> = text.lines().filter(|l| !l.is_empty()).collect();
    lines.sort();
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn grit_show_body(stdout: &str, out: Out) -> String {
    let pred: fn(&str) -> bool = match out {
        Out::Numstat => is_numstat_line,
        Out::NameStatus => is_name_status_line,
        Out::Json => |_| false,
    };
    let mut lines: Vec<_> = stdout.lines().filter(|l| pred(l)).collect();
    if out == Out::NameStatus {
        lines.sort();
    }
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    body
}

fn compare_text_case(fx: &Fixture, case: &Case) {
    let git_args = build_git_args(fx, case.mode, case.out, case.flags);
    let grit_args = build_grit_args(fx, case.mode, case.out, case.flags);

    let g = exec_grit(fx, grit_args);
    let r = exec_git(fx, git_args.clone());
    assert_eq!(
        g.status, r.status,
        "{}: exit grit={:?} git={:?}\ngrit stderr: {}\ngit stderr: {}",
        case.label, g.status, r.status, g.stderr, r.stderr
    );
    if g.ok() && r.ok() {
        let mut grit_stdout = if matches!(case.mode, Mode::Show) {
            grit_show_body(&g.stdout, case.out)
        } else {
            g.stdout.clone()
        };
        let mut git_stdout = r.stdout.clone();
        if case.out == Out::NameStatus {
            grit_stdout = sort_name_status_lines(&grit_stdout);
            git_stdout = sort_name_status_lines(&git_stdout);
        }
        assert_eq_nice!(
            grit_stdout,
            git_stdout,
            "{}: stdout mismatch\ngrit: {:?}\ngit: {:?}",
            case.label,
            build_grit_args(fx, case.mode, case.out, case.flags),
            git_args,
        );
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct NumstatLine {
    ins: String,
    del: String,
    path: String,
}

fn parse_git_numstat(text: &str) -> Vec<NumstatLine> {
    let mut lines: Vec<_> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut tabs = l.split('\t');
            let ins = tabs.next().unwrap_or("-").to_string();
            let del = tabs.next().unwrap_or("-").to_string();
            let path = tabs.next().unwrap_or("").to_string();
            NumstatLine { ins, del, path }
        })
        .collect();
    lines.sort();
    lines
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct NameStatusLine(String);

fn parse_git_name_status(text: &str) -> Vec<NameStatusLine> {
    let mut lines: Vec<_> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| NameStatusLine(l.to_string()))
        .collect();
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    lines
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct HunkRange {
    path: String,
    old_start: u64,
    old_lines: u64,
    new_start: u64,
    new_lines: u64,
}

fn parse_git_patch_hunks(text: &str) -> Vec<HunkRange> {
    let mut out = Vec::new();
    let mut current_path = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(path) = rest.split_whitespace().nth(1) {
                current_path = path.trim_start_matches("b/").to_string();
            }
            continue;
        }
        if let Some(header) = line.strip_prefix("@@ ") {
            let parts: Vec<_> = header.split_whitespace().collect();
            if parts.len() >= 2 {
                let old = parts[0].trim_start_matches('-');
                let new = parts[1].trim_start_matches('+');
                let (old_start, old_lines) = parse_range(old);
                let (new_start, new_lines) = parse_range(new);
                out.push(HunkRange {
                    path: current_path.clone(),
                    old_start,
                    old_lines,
                    new_start,
                    new_lines,
                });
            }
        }
    }
    out.sort();
    out
}

fn parse_range(spec: &str) -> (u64, u64) {
    let spec = spec.trim_end_matches(" @@").trim_end_matches('@');
    if let Some((start, len)) = spec.split_once(',') {
        (start.parse().unwrap_or(0), len.parse().unwrap_or(0))
    } else {
        (spec.parse().unwrap_or(0), 1)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct JsonFileSummary {
    path: String,
    old_path: Option<String>,
    ins: Option<String>,
    del: Option<String>,
    name_status: Option<String>,
    hunks: Vec<(u64, u64, u64, u64)>,
}

/// Context width flags do not affect numstat/name-status; git may print a patch if they are set.
fn flags_without_unified_context(flags: &[&str]) -> Vec<String> {
    flags
        .iter()
        .filter(|f| {
            !matches!(**f, "-U0" | "-U1" | "-U2" | "-U3")
                && !f.starts_with("--unified=")
                && **f != "--unified"
        })
        .map(|s| (*s).to_string())
        .collect()
}

fn flag_refs(flags: &[String]) -> Vec<&str> {
    flags.iter().map(String::as_str).collect()
}

fn normalized_git_numstat_path(path: &str) -> String {
    path.split_once(" => ")
        .map(|(_, dst)| dst.to_string())
        .unwrap_or_else(|| path.to_string())
}

fn grit_files_from_value(v: &Value) -> Vec<JsonFileSummary> {
    let files = v
        .get("files")
        .and_then(Value::as_array)
        .expect("files array");
    files.iter().map(grit_file_summary).collect()
}

fn grit_log_commits(v: &Value) -> Vec<(String, Vec<JsonFileSummary>)> {
    v.get("commits")
        .and_then(Value::as_array)
        .expect("commits")
        .iter()
        .map(|c| {
            let oid = c["oid"].as_str().unwrap().to_string();
            let files = c["files"]
                .as_array()
                .expect("commit files")
                .iter()
                .map(grit_file_summary)
                .collect();
            (oid, files)
        })
        .collect()
}

fn grit_file_summary(f: &Value) -> JsonFileSummary {
    let path = f["path"].as_str().unwrap().to_string();
    let ins = f
        .get("stat")
        .and_then(|s| s.get("insertions"))
        .map(|n| n.to_string());
    let del = f
        .get("stat")
        .and_then(|s| s.get("deletions"))
        .map(|n| n.to_string());
    let status = f.get("status").and_then(Value::as_str).map(String::from);
    let name_status = status.map(|s| match s.as_str() {
        "renamed" => {
            let old = f["old_path"].as_str().unwrap_or("");
            let sim = f["similarity"].as_u64().unwrap_or(100);
            format!("R{sim:03}\t{old}\t{path}")
        }
        "added" => format!("A\t{path}"),
        "deleted" => format!("D\t{path}"),
        "modified" => format!("M\t{path}"),
        other => panic!("unexpected status {other}"),
    });
    let hunks = f
        .get("hunks")
        .and_then(Value::as_array)
        .map(|hs| {
            hs.iter()
                .map(|h| {
                    (
                        h["old_start"].as_u64().unwrap_or(0),
                        h["old_lines"].as_u64().unwrap_or(0),
                        h["new_start"].as_u64().unwrap_or(0),
                        h["new_lines"].as_u64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    JsonFileSummary {
        path: path.clone(),
        old_path: f.get("old_path").and_then(Value::as_str).map(String::from),
        ins,
        del,
        name_status,
        hunks,
    }
}

fn compare_json_case(fx: &Fixture, case: &Case) {
    let g = exec_grit(fx, build_grit_args(fx, case.mode, Out::Json, case.flags));
    assert!(g.ok(), "{}: {}", case.label, g.dump("grit json"));

    let patch_out = exec_git(fx, build_git_args(fx, case.mode, Out::Json, case.flags));
    assert!(
        patch_out.ok(),
        "{}: {}",
        case.label,
        patch_out.dump("git patch")
    );

    let v: Value = serde_json::from_str(g.stdout.trim()).expect("grit json parse");

    match case.mode {
        Mode::Log => {
            let grit_commits = grit_log_commits(&v);
            assert_eq!(grit_commits.len(), 3, "{}: expected 3 commits", case.label);
            let stat_flag_strings = flags_without_unified_context(case.flags);
            let stat_flags = flag_refs(&stat_flag_strings);
            for (oid, grit_files) in grit_commits {
                let mut ns_args = vec![
                    "show".to_string(),
                    "--name-status".to_string(),
                    "--format=".to_string(),
                ];
                ns_args.extend(stat_flags.iter().map(|s| (*s).to_string()));
                ns_args.push(oid.clone());
                let git_show_ns = exec_git(fx, ns_args);
                assert!(git_show_ns.ok());
                let git_ns = parse_git_name_status(&git_show_ns.stdout);

                let mut num_args = vec![
                    "show".to_string(),
                    "--numstat".to_string(),
                    "--format=".to_string(),
                ];
                num_args.extend(stat_flags.iter().map(|s| (*s).to_string()));
                num_args.push(oid.clone());
                let git_show_num = exec_git(fx, num_args);
                assert!(git_show_num.ok());
                let git_num = parse_git_numstat(&git_show_num.stdout);

                let mut show_patch_args = vec!["show".into(), "--format=".into()];
                show_patch_args.extend(case.flags.iter().map(|s| (*s).to_string()));
                show_patch_args.push(oid.clone());
                let git_show_patch = exec_git(fx, show_patch_args);
                assert!(git_show_patch.ok());
                let git_hunks = parse_git_patch_hunks(&git_show_patch.stdout);

                assert_grit_files_match_git(
                    &grit_files,
                    &git_ns,
                    &git_num,
                    &git_hunks,
                    &format!("{} log {oid}", case.label),
                );
            }
        }
        Mode::Show => {
            let grit_files = grit_files_from_value(&v);
            let git_ns = parse_git_name_status(
                &exec_git(
                    fx,
                    vec![
                        "show".into(),
                        "--name-status".into(),
                        "--format=".into(),
                        "-M50".into(),
                        fx.c2.clone(),
                    ],
                )
                .stdout,
            );
            let git_num = parse_git_numstat(
                &exec_git(
                    fx,
                    vec![
                        "show".into(),
                        "--numstat".into(),
                        "--format=".into(),
                        fx.c2.clone(),
                    ],
                )
                .stdout,
            );
            let git_hunks = parse_git_patch_hunks(&patch_out.stdout);
            assert_grit_files_match_git(&grit_files, &git_ns, &git_num, &git_hunks, case.label);
        }
        _ => {
            let grit_files = grit_files_from_value(&v);
            let stat_flag_strings = flags_without_unified_context(case.flags);
            let stat_flags = flag_refs(&stat_flag_strings);
            let git_ns = parse_git_name_status(
                &exec_git(
                    fx,
                    build_git_args(fx, case.mode, Out::NameStatus, &stat_flags),
                )
                .stdout,
            );
            let git_num = parse_git_numstat(
                &exec_git(fx, build_git_args(fx, case.mode, Out::Numstat, &stat_flags)).stdout,
            );
            let git_hunks = parse_git_patch_hunks(&patch_out.stdout);
            assert_grit_files_match_git(&grit_files, &git_ns, &git_num, &git_hunks, case.label);
        }
    }
}

fn assert_grit_files_match_git(
    grit_files: &[JsonFileSummary],
    git_ns: &[NameStatusLine],
    git_num: &[NumstatLine],
    git_hunks: &[HunkRange],
    label: &str,
) {
    let mut grit_paths: Vec<_> = grit_files.iter().map(|f| f.path.clone()).collect();
    grit_paths.sort();
    let mut git_paths: Vec<_> = git_num
        .iter()
        .map(|n| normalized_git_numstat_path(&n.path))
        .filter(|p| !p.is_empty())
        .collect();
    if git_paths.is_empty() {
        git_paths = git_ns
            .iter()
            .filter_map(|l| l.0.split('\t').last().map(str::to_string))
            .collect();
    }
    git_paths.sort();
    assert_eq!(grit_paths, git_paths, "{label}: path set");

    for gf in grit_files {
        if let (Some(ins), Some(del)) = (&gf.ins, &gf.del) {
            let git_line = git_num
                .iter()
                .find(|n| {
                    normalized_git_numstat_path(&n.path) == gf.path
                        || n.path == gf.path
                        || gf
                            .old_path
                            .as_ref()
                            .is_some_and(|old| n.path.contains(old.as_str()))
                })
                .unwrap_or_else(|| panic!("{label}: numstat missing {}", gf.path));
            assert_eq!(ins, &git_line.ins, "{label}: ins {}", gf.path);
            assert_eq!(del, &git_line.del, "{label}: del {}", gf.path);
        }
        if let Some(ns) = &gf.name_status {
            assert!(
                git_ns.iter().any(|g| g.0 == *ns),
                "{label}: name-status {ns} not in git {git_ns:?}"
            );
        }
        for (old_start, old_lines, new_start, new_lines) in &gf.hunks {
            assert!(
                git_hunks.iter().any(|h| {
                    (h.path == gf.path || gf.old_path.as_ref().is_some_and(|old| h.path == *old))
                        && h.old_start == *old_start
                        && h.old_lines == *old_lines
                        && h.new_start == *new_start
                        && h.new_lines == *new_lines
                }),
                "{label}: hunk {:?} not found in git hunks {:?} for {}",
                (old_start, old_lines, new_start, new_lines),
                git_hunks,
                gf.path
            );
        }
    }
}

const CASES: &[Case] = &[
    // Two-commit diff: each flag at least once across modes; rename-aware cases here.
    Case {
        mode: Mode::TwoCommit,
        out: Out::Numstat,
        flags: &[],
        label: "two_commit numstat baseline",
    },
    Case {
        mode: Mode::TwoCommit,
        out: Out::NameStatus,
        flags: &["-M30"],
        label: "two_commit name-status -M30",
    },
    Case {
        mode: Mode::TwoCommit,
        out: Out::Json,
        flags: &["-U0"],
        label: "two_commit json -U0",
    },
    Case {
        mode: Mode::TwoCommit,
        out: Out::Numstat,
        flags: &["--no-renames"],
        label: "two_commit numstat --no-renames",
    },
    Case {
        mode: Mode::TwoCommit,
        out: Out::NameStatus,
        flags: &["--diff-filter=AM"],
        label: "two_commit name-status filter AM",
    },
    Case {
        mode: Mode::TwoCommit,
        out: Out::Json,
        flags: &["--diff-algorithm=patience"],
        label: "two_commit json patience",
    },
    // Staged
    Case {
        mode: Mode::Staged,
        out: Out::Json,
        flags: &["-w"],
        label: "staged json -w",
    },
    Case {
        mode: Mode::Staged,
        out: Out::Numstat,
        flags: &["-b"],
        label: "staged numstat -b",
    },
    Case {
        mode: Mode::Staged,
        out: Out::NameStatus,
        flags: &["--ignore-blank-lines"],
        label: "staged name-status ignore-blank-lines",
    },
    // Worktree
    Case {
        mode: Mode::Worktree,
        out: Out::Json,
        flags: &["-U1"],
        label: "worktree json -U1",
    },
    Case {
        mode: Mode::Worktree,
        out: Out::NameStatus,
        flags: &["--diff-filter=AM"],
        label: "worktree name-status filter AM",
    },
    Case {
        mode: Mode::Worktree,
        out: Out::Numstat,
        flags: &[],
        label: "worktree numstat baseline",
    },
    // show
    Case {
        mode: Mode::Show,
        out: Out::Numstat,
        flags: &["-M30"],
        label: "show numstat -M30",
    },
    Case {
        mode: Mode::Show,
        out: Out::NameStatus,
        flags: &["--no-renames"],
        label: "show name-status --no-renames",
    },
    Case {
        mode: Mode::Show,
        out: Out::Json,
        flags: &["-w"],
        label: "show json -w",
    },
    // log (JSON only — grit log human layout is not git-compatible)
    Case {
        mode: Mode::Log,
        out: Out::Json,
        flags: &["-M30"],
        label: "log json -M30",
    },
    Case {
        mode: Mode::Log,
        out: Out::Json,
        flags: &["--diff-algorithm=patience"],
        label: "log json patience",
    },
    Case {
        mode: Mode::Log,
        out: Out::Json,
        flags: &["-U0"],
        label: "log json -U0",
    },
    Case {
        mode: Mode::Log,
        out: Out::Json,
        flags: &["-b"],
        label: "log json -b",
    },
    Case {
        mode: Mode::Log,
        out: Out::Json,
        flags: &["--ignore-blank-lines"],
        label: "log json ignore-blank-lines",
    },
];

#[test]
fn diff_flag_matrix_matches_git() {
    let fx = setup_fixture();
    for case in CASES {
        match case.out {
            Out::Json => compare_json_case(&fx, case),
            Out::Numstat | Out::NameStatus => compare_text_case(&fx, case),
        }
    }
}
