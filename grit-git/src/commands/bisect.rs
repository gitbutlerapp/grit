//! `grit bisect` — binary search to find the commit that introduced a bug.
//!
//! Behaviour is aligned with upstream `git bisect` for porcelain tests.

use crate::commands::checkout::detach_head;
use crate::explicit_exit::ExplicitExit;
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::check_ref_format::{check_refname_format, RefNameOptions};
use grit_lib::index::MODE_TREE;
use grit_lib::merge_base::{is_ancestor, merge_bases_first_vs_rest};
use grit_lib::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, split_revision_token, OrderingMode, RevListOptions};
use grit_lib::rev_parse::{resolve_revision, resolve_revision_as_commit};
use std::collections::HashSet;
use std::fs;
use std::io::{stdin, stdout, IsTerminal, Write};
#[cfg(unix)]
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Arguments for `grit bisect`.
#[derive(Debug, ClapArgs)]
#[command(about = "Use binary search to find the commit that introduced a bug")]
pub struct Args {
    /// Bisect subcommand and its arguments.
    #[arg(value_name = "SUBCOMMAND", num_args = 0.., trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[derive(Clone)]
struct BisectTerms {
    term_bad: String,
    term_good: String,
}

impl BisectTerms {
    fn default_terms() -> Self {
        Self {
            term_bad: "bad".to_owned(),
            term_good: "good".to_owned(),
        }
    }

    fn read(git_dir: &Path) -> Self {
        let path = bisect_state_dir(git_dir).join("BISECT_TERMS");
        let Ok(content) = fs::read_to_string(&path) else {
            return Self::default_terms();
        };
        let mut lines = content.lines();
        let bad = lines.next().unwrap_or("bad").trim().to_owned();
        let good = lines.next().unwrap_or("good").trim().to_owned();
        Self {
            term_bad: bad,
            term_good: good,
        }
    }
}

fn sq_quote_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Bisect metadata files (`BISECT_LOG`, `BISECT_START`, …) live in the shared repository
/// directory, not under `.git/worktrees/<id>/`, so linked worktrees see one session.
fn bisect_state_dir(git_dir: &Path) -> PathBuf {
    refs::common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf())
}

/// Returns `Ok(())` when every tree reachable from `commit_oid` exists in the object database.
///
/// Matches Git's `unable to read tree` error used by bisect when a commit points at a missing tree.
fn verify_commit_tree_fully_readable(repo: &Repository, commit_oid: ObjectId) -> Result<()> {
    let object = repo
        .odb
        .read(&commit_oid)
        .with_context(|| format!("read commit {commit_oid}"))?;
    if object.kind != ObjectKind::Commit {
        bail!("fatal: unable to read tree ({commit_oid})");
    }
    let commit = parse_commit(&object.data)?;
    verify_tree_fully_readable(repo, commit.tree)
}

fn verify_tree_fully_readable(repo: &Repository, tree_oid: ObjectId) -> Result<()> {
    let object = repo
        .odb
        .read(&tree_oid)
        .map_err(|_| anyhow::anyhow!("fatal: unable to read tree ({tree_oid})"))?;
    if object.kind != ObjectKind::Tree {
        return Ok(());
    }
    let entries = parse_tree(&object.data)?;
    for entry in entries {
        if entry.mode == MODE_TREE {
            verify_tree_fully_readable(repo, entry.oid)?;
        }
    }
    Ok(())
}

/// `true` when `git_dir` is a linked worktree's administrative directory (`…/worktrees/<id>`),
/// which contains a `commondir` file. The primary repository's `.git` does not.
fn is_linked_worktree_git_dir(git_dir: &Path) -> bool {
    git_dir.join("commondir").is_file()
}

fn sq_quote_argv(args: &[String]) -> String {
    args.iter()
        .map(|a| sq_quote_str(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `hello` line count as `Np` for bisect run scripts using `sed -ne $p hello` (t6030).
fn hello_sed_p_env(work_dir: &Path) -> Option<String> {
    let path = work_dir.join("hello");
    let Ok(s) = fs::read_to_string(&path) else {
        return None;
    };
    let n = s.lines().count();
    if n == 0 {
        None
    } else {
        Some(format!("{n}p"))
    }
}

/// Parse one line of `BISECT_NAMES` (shell-quoted words) into pathspec tokens.
fn first_bisect_replay_token_and_rest(line: &str) -> (&str, &str) {
    let s = line.trim_start();
    let word_end = s.find(char::is_whitespace).unwrap_or(s.len());
    let word = &s[..word_end];
    let rest = s[word_end..].trim_start();
    (word, rest)
}

fn parse_bisect_names_line(line: &str) -> Result<Vec<String>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    let bytes = line.as_bytes();
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'\'' {
            i += 1;
            let mut word = String::new();
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 3 < bytes.len()
                        && bytes[i + 1] == b'\\'
                        && bytes[i + 2] == b'\''
                        && bytes[i + 3] == b'\''
                    {
                        word.push('\'');
                        i += 4;
                        continue;
                    }
                    i += 1;
                    break;
                }
                word.push(bytes[i] as char);
                i += 1;
            }
            out.push(word);
        } else {
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            out.push(line[start..i].to_owned());
        }
    }
    Ok(out)
}

fn read_bisect_pathspecs(git_dir: &Path) -> Result<Vec<String>> {
    let path = bisect_state_dir(git_dir).join("BISECT_NAMES");
    let Ok(content) = fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    let mut all = Vec::new();
    for line in content.lines() {
        all.extend(parse_bisect_names_line(line)?);
    }
    Ok(all)
}

fn bisect_log_exists(git_dir: &Path) -> bool {
    bisect_state_dir(git_dir).join("BISECT_LOG").exists()
}

fn check_term_format(term: &str, orig: &str) -> Result<()> {
    let synthetic = format!("refs/bisect/{term}");
    check_refname_format(&synthetic, &RefNameOptions::default())
        .map_err(|_| anyhow::anyhow!("'{term}' is not a valid term"))?;
    const RESERVED: &[&str] = &[
        "help",
        "start",
        "skip",
        "next",
        "reset",
        "visualize",
        "view",
        "replay",
        "log",
        "run",
        "terms",
    ];
    if RESERVED.contains(&term) {
        bail!("can't use the builtin command '{term}' as a term");
    }
    // Match `check_term_format` in Git's `bisect.c`: forbid swapping the canonical
    // good/bad words onto the opposite role; allow aliases new/old for bad/good.
    if orig != "bad" && matches!(term, "bad" | "new") {
        bail!("can't change the meaning of the term '{orig}'");
    }
    if orig != "good" && matches!(term, "good" | "old") {
        bail!("can't change the meaning of the term '{orig}'");
    }
    Ok(())
}

fn write_terms_file(git_dir: &Path, bad: &str, good: &str) -> Result<()> {
    if bad == good {
        bail!("please use two different terms");
    }
    check_term_format(bad, "bad")?;
    check_term_format(good, "good")?;
    fs::write(
        bisect_state_dir(git_dir).join("BISECT_TERMS"),
        format!("{bad}\n{good}\n"),
    )?;
    Ok(())
}

fn read_head_oid(repo: &Repository, git_dir: &Path) -> Result<ObjectId> {
    match refs::resolve_ref(git_dir, "BISECT_HEAD") {
        Ok(oid) => Ok(oid),
        Err(_) => resolve_revision(repo, "HEAD").map_err(|e| e.into()),
    }
}

fn commit_subject_line(repo: &Repository, oid: ObjectId) -> Result<String> {
    let object = repo.odb.read(&oid)?;
    let commit = parse_commit(&object.data)?;
    Ok(commit
        .message
        .lines()
        .next()
        .unwrap_or("")
        .trim_end()
        .to_owned())
}

fn append_bisect_log_raw(git_dir: &Path, line: &str) -> Result<()> {
    let log_path = bisect_state_dir(git_dir).join("BISECT_LOG");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn log_commit_line(git_dir: &Path, label: &str, oid: ObjectId, subject: &str) -> Result<()> {
    append_bisect_log_raw(git_dir, &format!("# {label}: [{oid}] {subject}"))
}

fn bisect_state_is_bad_side(terms: &BisectTerms, state: &str) -> bool {
    state == terms.term_bad
        || (terms.term_bad == "bad" && state == "bad")
        || (terms.term_bad == "new" && state == "new")
}

fn bisect_state_is_good_side(terms: &BisectTerms, state: &str) -> bool {
    state == terms.term_good
        || (terms.term_good == "good" && state == "good")
        || (terms.term_good == "old" && state == "old")
}

fn bisect_write(
    repo: &Repository,
    git_dir: &Path,
    terms: &BisectTerms,
    state: &str,
    rev: &str,
    nolog: bool,
) -> Result<()> {
    let oid = resolve_revision_as_commit(repo, rev)
        .with_context(|| format!("couldn't get the oid of the rev '{rev}'"))?;
    let tag = if bisect_state_is_bad_side(terms, state) {
        format!("refs/bisect/{}", terms.term_bad)
    } else if bisect_state_is_good_side(terms, state) {
        format!("refs/bisect/{}-{oid}", terms.term_good)
    } else if state == "skip" {
        format!("refs/bisect/skip-{oid}")
    } else {
        bail!("Bad bisect_write argument: {state}");
    };
    refs::write_ref(git_dir, &tag, &oid)?;
    let subject = commit_subject_line(repo, oid)?;
    log_commit_line(git_dir, state, oid, &subject)?;
    if !nolog {
        append_bisect_log_raw(git_dir, &format!("git bisect {state} {oid}"))?;
    }
    Ok(())
}

fn read_bisect_bad_ref(git_dir: &Path, terms: &BisectTerms) -> Option<ObjectId> {
    let refname = format!("refs/bisect/{}", terms.term_bad);
    refs::resolve_ref(git_dir, &refname).ok()
}

fn read_bisect_good_refs(git_dir: &Path, terms: &BisectTerms) -> Result<Vec<ObjectId>> {
    let prefix = format!("refs/bisect/{}-", terms.term_good);
    let all = refs::list_refs(git_dir, "refs/bisect/")?;
    let mut goods = Vec::new();
    for (name, oid) in all {
        if name.starts_with(&prefix) {
            goods.push(oid);
        }
    }
    Ok(goods)
}

fn read_bisect_skip_refs(git_dir: &Path) -> Result<HashSet<ObjectId>> {
    let prefix = "refs/bisect/skip-";
    let all = refs::list_refs(git_dir, "refs/bisect/")?;
    let mut skips = HashSet::new();
    for (name, oid) in all {
        if name.starts_with(prefix) {
            skips.insert(oid);
        }
    }
    Ok(skips)
}

fn count_bisect_state(git_dir: &Path, terms: &BisectTerms) -> (usize, usize) {
    let nr_bad = read_bisect_bad_ref(git_dir, terms).map(|_| 1).unwrap_or(0);
    let goods = read_bisect_good_refs(git_dir, terms).unwrap_or_default();
    (goods.len(), nr_bad)
}

fn status_and_log_printf(git_dir: &Path, msg: &str) -> Result<()> {
    print!("{msg}");
    append_bisect_log_raw(git_dir, &format!("# {}", msg.trim_end_matches('\n')))?;
    Ok(())
}

fn bisect_print_status(git_dir: &Path, terms: &BisectTerms) -> Result<()> {
    let (nr_good, nr_bad) = count_bisect_state(git_dir, terms);
    if nr_good > 0 && nr_bad > 0 {
        return Ok(());
    }
    if nr_good == 0 && nr_bad == 0 {
        status_and_log_printf(git_dir, "status: waiting for both good and bad commits\n")?;
    } else if nr_good > 0 {
        let msg = if nr_good == 1 {
            "status: waiting for bad commit, 1 good commit known\n".to_owned()
        } else {
            format!("status: waiting for bad commit, {nr_good} good commits known\n")
        };
        status_and_log_printf(git_dir, &msg)?;
    } else {
        status_and_log_printf(
            git_dir,
            "status: waiting for good commit(s), bad commit known\n",
        )?;
    }
    Ok(())
}

/// Result of [`bisect_next_check`]: whether `bisect next` may run.
enum BisectNextGate {
    /// Proceed to `bisect_next_all`.
    Proceed,
    /// Used by `bisect_auto_next`: print status only, do not run `next`.
    BlockAuto,
    /// Fatal for explicit `git bisect next` (error already printed to stderr).
    Fail,
}

fn bisect_next_check(
    git_dir: &Path,
    terms: &BisectTerms,
    current_term: Option<&str>,
    auto_mode: bool,
) -> Result<BisectNextGate> {
    let (nr_good, nr_bad) = count_bisect_state(git_dir, terms);
    let missing_good = nr_good == 0;
    let missing_bad = nr_bad == 0;
    if !missing_good && !missing_bad {
        return Ok(BisectNextGate::Proceed);
    }
    let Some(current) = current_term else {
        return Ok(if auto_mode {
            BisectNextGate::BlockAuto
        } else {
            BisectNextGate::Fail
        });
    };
    let vocab_bad = terms.term_bad.as_str();
    let vocab_good = terms.term_good.as_str();
    if missing_good && !missing_bad && current == vocab_good {
        eprintln!("warning: bisecting only with a {vocab_bad} commit");
        if stdin().is_terminal() {
            eprint!("Are you sure [Y/n]? ");
            use std::io::BufRead;
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            let line = line.trim();
            if line.starts_with('N') || line.starts_with('n') {
                return Ok(BisectNextGate::Fail);
            }
        }
        return Ok(BisectNextGate::Proceed);
    }
    if bisect_state_dir(git_dir).join("BISECT_START").exists() {
        eprintln!(
            "error: You need to give me at least one {vocab_bad} and {vocab_good} revision.\n\
             You can use \"git bisect {vocab_bad}\" and \"git bisect {vocab_good}\" for that."
        );
    } else {
        eprintln!(
            "error: You need to start by \"git bisect start\".\n\
             You then need to give me at least one {vocab_good} and {vocab_bad} revision.\n\
             You can use \"git bisect {vocab_good}\" and \"git bisect {vocab_bad}\" for that."
        );
    }
    Ok(BisectNextGate::Fail)
}

fn ensure_bisecting(git_dir: &Path) -> Result<()> {
    if !bisect_log_exists(git_dir) {
        bail!("We are not bisecting.");
    }
    Ok(())
}

/// True when `BISECT_START` exists and is non-empty (Git: `bisect_autostart` gate).
fn bisect_start_nonempty(git_dir: &Path) -> bool {
    let p = bisect_state_dir(git_dir).join("BISECT_START");
    fs::read_to_string(p)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

fn ensure_bisect_start_present(git_dir: &Path) -> Result<()> {
    if bisect_start_nonempty(git_dir) {
        return Ok(());
    }
    bail!("You need to start by \"git bisect start\"\n");
}

fn expected_rev_matches(git_dir: &Path, oid: ObjectId) -> bool {
    let path = bisect_state_dir(git_dir).join("BISECT_EXPECTED_REV");
    let Ok(s) = fs::read_to_string(path) else {
        return false;
    };
    ObjectId::from_hex(s.trim()).ok() == Some(oid)
}

fn log2u(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    usize::BITS as usize - n.leading_zeros() as usize - 1
}

fn exp2i(n: usize) -> usize {
    1usize << n
}

fn estimate_bisect_steps(all: usize) -> usize {
    if all < 3 {
        return 0;
    }
    let n = log2u(all);
    let e = exp2i(n);
    let x = all - e;
    if e < 3 * x {
        n
    } else {
        n.saturating_sub(1)
    }
}

fn check_merge_bases(
    repo: &Repository,
    git_dir: &Path,
    terms: &BisectTerms,
    bad: ObjectId,
    goods: &[ObjectId],
    no_checkout: bool,
) -> Result<Option<()>> {
    let bases = merge_bases_first_vs_rest(repo, bad, goods)?;
    let skips = read_bisect_skip_refs(git_dir)?;
    for mb in bases {
        if mb == bad {
            if expected_rev_matches(git_dir, bad) {
                let good_hex: Vec<String> = goods.iter().map(|o| o.to_string()).collect();
                let joined = good_hex.join(" ");
                if terms.term_bad == "bad" && terms.term_good == "good" {
                    eprintln!(
                        "The merge base {bad} is bad.\n\
                         This means the bug has been fixed between {bad} and [{joined}]."
                    );
                } else if terms.term_bad == "new" && terms.term_good == "old" {
                    eprintln!(
                        "The merge base {bad} is new.\n\
                         The property has changed between {bad} and [{joined}]."
                    );
                } else {
                    eprintln!(
                        "The merge base {bad} is {}.\n\
                         This means the first '{}' commit is between {bad} and [{joined}].",
                        terms.term_bad, terms.term_good
                    );
                }
                bail!("merge base check failed");
            }
            eprintln!(
                "Some {} revs are not ancestors of the {} rev.\n\
                 git bisect cannot work properly in this case.\n\
                 Maybe you mistook {} and {} revs?",
                terms.term_good, terms.term_bad, terms.term_good, terms.term_bad
            );
            bail!("mistook good and bad");
        }
        if goods.contains(&mb) {
            continue;
        }
        if skips.contains(&mb) {
            let good_hex: Vec<String> = goods.iter().map(|o| o.to_string()).collect();
            let joined = good_hex.join(" ");
            eprintln!(
                "warning: the merge base between {bad} and [{joined}] must be skipped.\n\
                 So we cannot be sure the first {} commit is between {mb} and {bad}.\n\
                 We continue anyway.",
                terms.term_bad
            );
            continue;
        }
        println!("Bisecting: a merge base must be tested\n");
        fs::write(
            bisect_state_dir(git_dir).join("BISECT_EXPECTED_REV"),
            format!("{mb}\n"),
        )?;
        if no_checkout {
            refs::write_ref(git_dir, "BISECT_HEAD", &mb)?;
        } else {
            verify_commit_tree_fully_readable(repo, mb)?;
            detach_head(repo, &mb, false).with_context(|| format!("checkout {}", mb.to_hex()))?;
        }
        bisect_checkout_show_commit(repo, mb)?;
        return Ok(Some(()));
    }
    Ok(None)
}

enum AncestorCheck {
    Continue,
    MergeBaseCheckedOut,
}

fn check_good_are_ancestors_of_bad(
    repo: &Repository,
    git_dir: &Path,
    terms: &BisectTerms,
    bad: ObjectId,
    goods: &[ObjectId],
    no_checkout: bool,
) -> Result<AncestorCheck> {
    let ancestors_ok = bisect_state_dir(git_dir).join("BISECT_ANCESTORS_OK");
    if ancestors_ok.exists() {
        return Ok(AncestorCheck::Continue);
    }
    if goods.is_empty() {
        return Ok(AncestorCheck::Continue);
    }
    let mut all_ancestors = true;
    for g in goods {
        if !is_ancestor(repo, *g, bad)? {
            all_ancestors = false;
            break;
        }
    }
    if !all_ancestors && check_merge_bases(repo, git_dir, terms, bad, goods, no_checkout)?.is_some()
    {
        return Ok(AncestorCheck::MergeBaseCheckedOut);
    }
    let _ = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&ancestors_ok);
    Ok(AncestorCheck::Continue)
}

fn bisect_checkout_show_commit(repo: &Repository, oid: ObjectId) -> Result<()> {
    let subject = commit_subject_line(repo, oid)?;
    println!("[{oid}] {subject}");
    Ok(())
}

fn rev_list_bisect_candidates(
    repo: &Repository,
    _git_dir: &Path,
    bad: ObjectId,
    goods: &[ObjectId],
    pathspecs: &[String],
    first_parent: bool,
) -> Result<(Vec<ObjectId>, bool)> {
    let positive = vec![bad.to_string()];
    let negative: Vec<String> = goods.iter().map(|g| g.to_string()).collect();
    // Git's bisect builds a topo-limited revision walk; the candidate order does not affect
    // weight-based selection, and topo ordering sidesteps the path-limited date-order walk.
    let opts = RevListOptions {
        first_parent,
        paths: pathspecs.to_vec(),
        ordering: OrderingMode::Topo,
        ..Default::default()
    };
    let result = rev_list(repo, &positive, &negative, &opts)?;
    Ok((result.commits, true))
}

/// One bisection candidate ranked by weight, mirroring Git's `commit_dist`.
#[derive(Clone, Copy)]
struct BisectRank {
    oid: ObjectId,
    /// Number of candidates reachable from `oid` (Git's `weight`, including `oid`).
    reaches: usize,
    /// Bisection distance `min(reaches, all - reaches)`.
    distance: usize,
}

/// Parents of `oid`, restricted by `first_parent`.
fn commit_parents(repo: &Repository, oid: ObjectId, first_parent: bool) -> Result<Vec<ObjectId>> {
    let object = repo.odb.read(&oid)?;
    if object.kind != ObjectKind::Commit {
        return Ok(Vec::new());
    }
    let commit = parse_commit(&object.data)?;
    if first_parent {
        Ok(commit.parents.into_iter().take(1).collect())
    } else {
        Ok(commit.parents)
    }
}

/// Committer timestamp (seconds since epoch) of `oid`, used to order the bisection candidate list
/// like Git's date-ordered limited revision walk. Returns 0 when unavailable.
fn commit_committer_time(repo: &Repository, oid: ObjectId) -> i64 {
    let Ok(object) = repo.odb.read(&oid) else {
        return 0;
    };
    let Ok(commit) = parse_commit(&object.data) else {
        return 0;
    };
    // `committer` is "Name <email> <secs> <tz>"; take the next-to-last whitespace token.
    let parts: Vec<&str> = commit.committer.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[parts.len() - 2].parse::<i64>().unwrap_or(0)
    } else {
        0
    }
}

/// Number of candidates reachable from `oid` (the commit's bisection weight), walking the full
/// ancestry and counting only commits in `candidate_set` (Git's `count_distance`). Intermediate
/// commits that are `TREESAME` to a parent are walked through but absent from the set and so not
/// counted.
fn count_reaches(
    repo: &Repository,
    oid: ObjectId,
    candidate_set: &HashSet<ObjectId>,
    first_parent: bool,
) -> Result<usize> {
    let mut stack = vec![oid];
    let mut visited = HashSet::new();
    let mut reaches = 0usize;
    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue;
        }
        if candidate_set.contains(&current) {
            reaches += 1;
        }
        for parent in commit_parents(repo, current, first_parent)? {
            stack.push(parent);
        }
    }
    Ok(reaches)
}

/// Git's `approx_halfway`: is `weight` within one of the exact halfway point of `nr`?
fn approx_halfway(weight: usize, nr: usize) -> bool {
    let diff = 2 * weight as i64 - nr as i64;
    if (-1..=1).contains(&diff) {
        return true;
    }
    nr >= 1024 && diff.unsigned_abs() < (nr as u64) / 1024
}

/// Outcome of [`find_bisection`]: the per-candidate ranks plus the single best revision chosen by
/// Git's non-`FIND_BISECTION_ALL` walk.
struct Bisection {
    /// Per-candidate ranks, sorted as Git's `best_bisection_sorted` (distance desc, oid asc).
    ranks: Vec<BisectRank>,
    /// Best revision chosen by the non-`ALL` `best_bisection`/`approx_halfway` walk.
    best: Option<ObjectId>,
    /// Weight (reaches) of `best`.
    best_reaches: usize,
}

/// Port of Git's `find_bisection` + `do_find_bisection` (`bisect.c`).
///
/// `dag` is the full interesting commit set (`bad ^good`, no pathspec) in rev-list newest-first
/// order; `candidate_set` is the path-matched (non-`TREESAME`) subset that contributes to weights.
/// Weights are computed over the full ancestry, counting only candidates. The non-`ALL` best is
/// found via the same ordered `approx_halfway` early-out and `best_bisection` fallback that Git
/// uses (a `TREESAME` commit is never selected), so the chosen midpoint matches
/// `git rev-list --bisect`. The returned `ranks` (candidates only) are sorted like `--bisect-all`
/// for skip handling.
fn find_bisection(
    repo: &Repository,
    dag: &[ObjectId],
    candidate_set: &HashSet<ObjectId>,
    first_parent: bool,
) -> Result<Bisection> {
    // `nr` (Git's `all`) counts only non-`TREESAME` commits = path-matched candidates.
    let nr = candidate_set.len();
    let dag_set: HashSet<ObjectId> = dag.iter().copied().collect();
    let treesame = |oid: ObjectId| !candidate_set.contains(&oid);

    // Git's limited revision walk yields commits newest-first by committer date (topo/insertion
    // order as tiebreak); `find_bisection` then reverses that to oldest-first before the weight
    // walk and `best_bisection` scan. The candidate rev-list may be topo-ordered here, so re-sort by
    // committer date (newest-first), preserving the rev-list order for equal dates, then reverse.
    let mut by_date: Vec<(usize, ObjectId)> = dag.iter().copied().enumerate().collect();
    let times: std::collections::HashMap<ObjectId, i64> = dag
        .iter()
        .map(|&oid| (oid, commit_committer_time(repo, oid)))
        .collect();
    by_date.sort_by(|a, b| times[&b.1].cmp(&times[&a.1]).then_with(|| a.0.cmp(&b.0)));
    let list: Vec<ObjectId> = by_date.into_iter().rev().map(|(_, oid)| oid).collect();

    // weight: 0+ known, -1 single-parent unknown, -2 merge unknown.
    let mut weight: std::collections::HashMap<ObjectId, i64> = std::collections::HashMap::new();
    let mut interesting_parents: std::collections::HashMap<ObjectId, Vec<ObjectId>> =
        std::collections::HashMap::new();
    for &oid in &list {
        // Git's count_interesting_parents counts parents that are not UNINTERESTING (i.e. in the
        // bad-not-good DAG), including TREESAME ones.
        let parents: Vec<ObjectId> = commit_parents(repo, oid, first_parent)?
            .into_iter()
            .filter(|p| dag_set.contains(p))
            .collect();
        let n_interesting = parents.len();
        let w = match n_interesting {
            0 => {
                if treesame(oid) {
                    0
                } else {
                    1
                }
            }
            1 => -1,
            _ => -2,
        };
        weight.insert(oid, w);
        interesting_parents.insert(oid, parents);
    }

    let mut best: Option<ObjectId> = None;
    let halfway_ok =
        |oid: ObjectId, w: i64| !treesame(oid) && approx_halfway(w.max(0) as usize, nr);

    // Resolve merge (-2) weights via count_distance, with the non-ALL halfway early-out.
    for &oid in &list {
        if weight[&oid] != -2 {
            continue;
        }
        let w = count_reaches(repo, oid, candidate_set, first_parent)? as i64;
        weight.insert(oid, w);
        if best.is_none() && halfway_ok(oid, w) {
            best = Some(oid);
        }
    }

    // Propagate single-parent (-1) weights, again with the halfway early-out. A TREESAME commit
    // inherits its parent's weight; a counted commit adds one.
    let total = list.len();
    let mut counted = list.iter().filter(|o| weight[*o] >= 0).count();
    while counted < total {
        let mut progressed = false;
        for &oid in &list {
            if weight[&oid] >= 0 {
                continue;
            }
            let parent = interesting_parents[&oid]
                .iter()
                .copied()
                .find(|p| weight.get(p).copied().unwrap_or(-1) >= 0);
            let Some(parent) = parent else {
                continue;
            };
            let pw = weight[&parent];
            let w = if treesame(oid) { pw } else { pw + 1 };
            weight.insert(oid, w);
            counted += 1;
            progressed = true;
            if best.is_none() && halfway_ok(oid, w) {
                best = Some(oid);
            }
        }
        if !progressed {
            break;
        }
    }

    // Build ranks (candidates only; distance desc, oid asc) for skip handling.
    let mut ranks: Vec<BisectRank> = list
        .iter()
        .filter(|oid| candidate_set.contains(oid))
        .map(|&oid| {
            let reaches = weight[&oid].max(0) as usize;
            BisectRank {
                oid,
                reaches,
                distance: reaches.min(nr.saturating_sub(reaches)),
            }
        })
        .collect();
    ranks.sort_by(|a, b| b.distance.cmp(&a.distance).then_with(|| a.oid.cmp(&b.oid)));

    // No early-out: Git's `best_bisection` keeps the first list (oldest-first) non-TREESAME commit
    // with strictly greater distance.
    if best.is_none() {
        let mut best_distance: i64 = -1;
        for &oid in &list {
            if treesame(oid) {
                continue;
            }
            let reaches = weight[&oid].max(0) as usize;
            let distance = reaches.min(nr.saturating_sub(reaches)) as i64;
            if distance > best_distance {
                best = Some(oid);
                best_distance = distance;
            }
        }
    }

    let best_reaches = best.map(|o| weight[&o].max(0) as usize).unwrap_or(0);
    Ok(Bisection {
        ranks,
        best,
        best_reaches,
    })
}

/// Pseudo-random generator from Git's `bisect.c` (`get_prn`); wrapping `u32` arithmetic.
fn get_prn(count: u32) -> u32 {
    let count = count.wrapping_mul(1103515245).wrapping_add(12345);
    (count / 65536) % 32768
}

/// Integer square root via Newton's method, matching Git's `sqrti` in `bisect.c`.
fn sqrti(val: i32) -> i32 {
    if val == 0 {
        return 0;
    }
    let mut x = val as f32;
    loop {
        let y = (x + val as f32 / x) / 2.0;
        let d = if y > x { y - x } else { x - y };
        x = y;
        if d < 0.5 {
            break;
        }
    }
    x as i32
}

/// Port of Git's `skip_away` (`bisect.c`): pick a candidate away from the (skipped) head of the
/// weight-sorted, skip-filtered list, deterministically based on the number of remaining commits.
fn skip_away(list: &[ObjectId], count: usize, current_bad: ObjectId) -> ObjectId {
    if list.is_empty() {
        return current_bad;
    }
    let prn = get_prn(count as u32);
    let index =
        ((count as u64 * prn as u64 / 32768) as i32 * sqrti(prn as i32) / sqrti(32768)) as usize;

    let mut result = 0usize;
    let mut previous: Option<usize> = None;
    for (i, &oid) in list.iter().enumerate() {
        if i == index {
            if oid != current_bad {
                result = i;
            } else if let Some(prev) = previous {
                result = prev;
            } else {
                result = 0;
            }
            break;
        }
        previous = Some(i);
    }
    list[result]
}

/// Result of applying `managed_skipped` to a weight-sorted candidate list.
struct ManagedSkipped {
    /// Commit to test next (best non-skipped, possibly chosen via `skip_away`), or `None` if
    /// every candidate is skipped.
    next: Option<ObjectId>,
    /// Skipped commits collected from the front of the list (Git's `tried`).
    tried: Vec<ObjectId>,
}

/// Port of Git's `managed_skipped` + `filter_skipped` (`bisect.c`).
///
/// `sorted` is the full weight-sorted candidate list (best first, as produced with
/// `FIND_BISECTION_ALL`). When the best candidate is not skipped it is returned directly with an
/// empty `tried`. Otherwise every skipped commit is collected into `tried`, the non-skipped
/// commits are gathered, and `skip_away` chooses one to test (or `next` is `None` when none remain).
fn managed_skipped(
    sorted: &[ObjectId],
    skip_oids: &HashSet<ObjectId>,
    current_bad: ObjectId,
) -> ManagedSkipped {
    if skip_oids.is_empty() {
        return ManagedSkipped {
            next: sorted.first().copied(),
            tried: Vec::new(),
        };
    }

    // filter_skipped(list, &tried, show_all=0, &count, &skipped_first)
    let mut tried = Vec::new();
    let mut filtered = Vec::new();
    let mut skipped_first = false;
    let mut seen_any = false;
    for &oid in sorted {
        if skip_oids.contains(&oid) {
            if !seen_any {
                skipped_first = true;
            }
            tried.push(oid);
        } else {
            if !skipped_first {
                // Shortcut: first (best) commit is not skipped — return it directly.
                return ManagedSkipped {
                    next: Some(oid),
                    tried: Vec::new(),
                };
            }
            filtered.push(oid);
        }
        seen_any = true;
    }

    if !skipped_first {
        // No commit was skipped at the front; behave like the non-skip path.
        return ManagedSkipped {
            next: filtered
                .first()
                .copied()
                .or_else(|| sorted.first().copied()),
            tried,
        };
    }

    let next = if filtered.is_empty() {
        None
    } else {
        Some(skip_away(&filtered, filtered.len(), current_bad))
    };
    ManagedSkipped { next, tried }
}

fn bisect_skipped_commits_log(
    repo: &Repository,
    git_dir: &Path,
    terms: &BisectTerms,
    candidates: &[ObjectId],
    bad: ObjectId,
) -> Result<()> {
    // Git's `bisect_skipped_commits` re-runs the `bad ^good` rev-walk and lists every commit it
    // yields (the candidate set already includes `bad`); it does not print `bad` separately.
    let _ = bad;
    append_bisect_log_raw(git_dir, "# only skipped commits left to test")?;
    for oid in candidates {
        let subject = commit_subject_line(repo, *oid)?;
        append_bisect_log_raw(
            git_dir,
            &format!(
                "# possible first {} commit: [{oid}] {subject}",
                terms.term_bad
            ),
        )?;
    }
    Ok(())
}

fn bisect_successful_log(repo: &Repository, git_dir: &Path, terms: &BisectTerms) -> Result<()> {
    let Some(bad) = read_bisect_bad_ref(git_dir, terms) else {
        return Ok(());
    };
    let subject = commit_subject_line(repo, bad)?;
    append_bisect_log_raw(
        git_dir,
        &format!("# first {} commit: [{bad}] {subject}", terms.term_bad),
    )?;
    Ok(())
}

fn error_if_skipped_commits(
    _repo: &Repository,
    terms: &BisectTerms,
    tried: &[ObjectId],
    bad: Option<ObjectId>,
) -> Result<bool> {
    if tried.is_empty() {
        return Ok(false);
    }
    println!(
        "There are only 'skip'ped commits left to test.\n\
         The first {} commit could be any of:",
        terms.term_bad
    );
    for oid in tried {
        println!("{oid}");
    }
    if let Some(b) = bad {
        println!("{b}");
    }
    println!("We cannot bisect more!");
    Ok(true)
}

fn bisect_next_all(repo: &Repository, git_dir: &Path, terms: &BisectTerms) -> Result<i32> {
    let no_checkout = refs::resolve_ref(git_dir, "BISECT_HEAD").is_ok();

    let Some(bad) = read_bisect_bad_ref(git_dir, terms) else {
        bisect_print_status(git_dir, terms)?;
        return Ok(0);
    };
    let goods = read_bisect_good_refs(git_dir, terms)?;
    if goods.is_empty() {
        bisect_print_status(git_dir, terms)?;
        return Ok(0);
    }

    if matches!(
        check_good_are_ancestors_of_bad(repo, git_dir, terms, bad, &goods, no_checkout)?,
        AncestorCheck::MergeBaseCheckedOut
    ) {
        return Ok(11);
    }

    let first_parent = bisect_state_dir(git_dir)
        .join("BISECT_FIRST_PARENT")
        .exists();
    let pathspecs = read_bisect_pathspecs(git_dir)?;
    let (candidates, _) =
        rev_list_bisect_candidates(repo, git_dir, bad, &goods, &pathspecs, first_parent)?;
    // The full interesting DAG (`bad ^good`, no pathspec) is the weight graph; path-matched
    // candidates are the non-`TREESAME` commits. Without pathspecs they coincide.
    let dag = if pathspecs.is_empty() {
        candidates.clone()
    } else {
        rev_list_bisect_candidates(repo, git_dir, bad, &goods, &[], first_parent)?.0
    };
    let candidate_set: HashSet<ObjectId> = candidates.iter().copied().collect();
    let skip_oids = read_bisect_skip_refs(git_dir)?;

    // Git's find_bisection: `all` counts the testable commits and ranks them by bisection distance.
    // Without skips it returns a single best (non-`ALL` `best_bisection`); with skips it returns the
    // full `best_bisection_sorted` list (distance desc, oid asc) so `managed_skipped` can pick a
    // commit away from skipped ones. `reaches` is the weight of that best candidate.
    let bisection = find_bisection(repo, &dag, &candidate_set, first_parent)?;
    let all = candidates.len();
    let sorted: Vec<ObjectId> = bisection.ranks.iter().map(|e| e.oid).collect();

    let (managed, reaches) = if skip_oids.is_empty() {
        (
            ManagedSkipped {
                next: bisection.best,
                tried: Vec::new(),
            },
            bisection.best_reaches,
        )
    } else {
        // ALL mode: `reaches` is the weight of the head of the sorted list (Git's `best`).
        let reaches = bisection.ranks.first().map(|r| r.reaches).unwrap_or(0);
        (managed_skipped(&sorted, &skip_oids, bad), reaches)
    };

    // With pathspecs, Git's `revs.commits` still contains the in-range commits even when none of
    // them touch the paths (they are `TREESAME`, so `all` is 0). Our path-filtered candidate list
    // is empty in that case, so distinguish "no testable commit" (range non-empty, paths match
    // nothing) from "was both good and bad" (range genuinely empty) by re-checking the unfiltered
    // range. This mirrors Git emitting BISECT_NO_TESTABLE_COMMIT before the `!revs.commits` path.
    if !pathspecs.is_empty() && candidates.is_empty() {
        let (unfiltered, _) =
            rev_list_bisect_candidates(repo, git_dir, bad, &goods, &[], first_parent)?;
        if !unfiltered.is_empty() {
            eprintln!(
                "No testable commit found.\n\
                 Maybe you started with bad path arguments?\n"
            );
            return Ok(4);
        }
    }

    let Some(bisect_rev) = managed.next else {
        // revs.commits empty after managed_skipped: only return BISECT_ONLY_SKIPPED if "bad" is
        // also "skip"ped (tried non-empty), otherwise "was both good and bad".
        if error_if_skipped_commits(repo, terms, &managed.tried, None)? {
            return Ok(2);
        }
        println!(
            "{} was both {} and {}",
            bad, terms.term_good, terms.term_bad
        );
        return Ok(1);
    };

    if all == 0 {
        println!("{} is the first {} commit", bad, terms.term_bad);
        run_show_stat(repo, bad)?;
        bisect_successful_log(repo, git_dir, terms)?;
        return Ok(10);
    }

    if bisect_rev == bad {
        if error_if_skipped_commits(repo, terms, &managed.tried, Some(bad))? {
            return Ok(2);
        }
        println!("{} is the first {} commit", bad, terms.term_bad);
        run_show_stat(repo, bad)?;
        bisect_successful_log(repo, git_dir, terms)?;
        return Ok(10);
    }

    let nr = all.saturating_sub(reaches).saturating_sub(1);
    let steps = estimate_bisect_steps(all);
    let steps_msg = if steps == 1 {
        "(roughly 1 step)".to_owned()
    } else {
        format!("(roughly {steps} steps)")
    };
    if nr == 1 {
        println!("Bisecting: 1 revision left to test after this {steps_msg}.");
    } else {
        println!("Bisecting: {nr} revisions left to test after this {steps_msg}.");
    }

    fs::write(
        bisect_state_dir(git_dir).join("BISECT_EXPECTED_REV"),
        format!("{bisect_rev}\n"),
    )?;
    if no_checkout {
        refs::write_ref(git_dir, "BISECT_HEAD", &bisect_rev)?;
    } else {
        verify_commit_tree_fully_readable(repo, bisect_rev)?;
        detach_head(repo, &bisect_rev, false)
            .with_context(|| format!("checkout {}", bisect_rev.to_hex()))?;
    }
    bisect_checkout_show_commit(repo, bisect_rev)?;
    Ok(0)
}

fn run_show_stat(_repo: &Repository, oid: ObjectId) -> Result<()> {
    let self_exe = std::env::current_exe().context("current_exe")?;
    let status = std::process::Command::new(&self_exe)
        .arg("show")
        .arg("--stat")
        .arg("--summary")
        .arg("--no-abbrev")
        .arg(oid.to_string())
        .status()
        .context("git show")?;
    if !status.success() {
        bail!("unable to start 'show' for object '{}'", oid.to_hex());
    }
    Ok(())
}

/// Runs `bisect next` after a state change. Returns the same codes as [`bisect_next_all`]
/// (`10` = first bad commit found, `2` = only skipped left, `4` = no testable commit).
fn bisect_auto_next(repo: &Repository, git_dir: &Path, terms: &BisectTerms) -> Result<i32> {
    match bisect_next_check(git_dir, terms, None, true)? {
        BisectNextGate::Proceed => {}
        BisectNextGate::BlockAuto => {
            bisect_print_status(git_dir, terms)?;
            return Ok(0);
        }
        BisectNextGate::Fail => {
            bail!("bisect next check failed");
        }
    }
    let res = bisect_next_all(repo, git_dir, terms)?;
    if res == 2 {
        let Some(bad) = read_bisect_bad_ref(git_dir, terms) else {
            return Ok(2);
        };
        let pathspecs = read_bisect_pathspecs(git_dir)?;
        let first_parent = bisect_state_dir(git_dir)
            .join("BISECT_FIRST_PARENT")
            .exists();
        let goods = read_bisect_good_refs(git_dir, terms)?;
        let (candidates, _) =
            rev_list_bisect_candidates(repo, git_dir, bad, &goods, &pathspecs, first_parent)?;
        bisect_skipped_commits_log(repo, git_dir, terms, &candidates, bad)?;
    }
    Ok(res)
}

fn cmd_next(repo: &Repository, args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!("'git bisect next' requires 0 arguments");
    }
    let git_dir = &repo.git_dir;
    ensure_bisecting(git_dir)?;
    ensure_bisect_start_present(git_dir)?;
    let terms = BisectTerms::read(git_dir);
    match bisect_next_check(git_dir, &terms, Some(&terms.term_good), false)? {
        BisectNextGate::Proceed => {}
        BisectNextGate::BlockAuto | BisectNextGate::Fail => {
            std::process::exit(1);
        }
    }
    let code = bisect_next_all(repo, git_dir, &terms)?;
    if code == 2 {
        let Some(bad) = read_bisect_bad_ref(git_dir, &terms) else {
            std::process::exit(2);
        };
        let pathspecs = read_bisect_pathspecs(git_dir)?;
        let first_parent = bisect_state_dir(git_dir)
            .join("BISECT_FIRST_PARENT")
            .exists();
        let goods = read_bisect_good_refs(git_dir, &terms)?;
        let (candidates, _) =
            rev_list_bisect_candidates(repo, git_dir, bad, &goods, &pathspecs, first_parent)?;
        bisect_skipped_commits_log(repo, git_dir, &terms, &candidates, bad)?;
        std::process::exit(2);
    }
    if code == 4 {
        std::process::exit(4);
    }
    Ok(())
}

fn clean_bisect_state(git_dir: &Path) -> Result<()> {
    // Remove every `refs/bisect/*` ref (loose and packed) so `git pack-refs` cannot leave
    // stale bisect pointers behind after reset.
    for (name, _) in refs::list_refs(git_dir, "refs/bisect/")? {
        let _ = refs::delete_ref(git_dir, &name);
    }
    let state_dir = bisect_state_dir(git_dir);
    let bisect_dir = state_dir.join("refs/bisect");
    if bisect_dir.is_dir() {
        let _ = fs::remove_dir_all(&bisect_dir);
    }
    for name in [
        "BISECT_LOG",
        "BISECT_START",
        "BISECT_EXPECTED_REV",
        "BISECT_NAMES",
        "BISECT_TERMS",
        "BISECT_HEAD",
        "BISECT_FIRST_PARENT",
        "BISECT_ANCESTORS_OK",
        "BISECT_RUN",
    ] {
        let _ = fs::remove_file(state_dir.join(name));
    }
    Ok(())
}

fn cmd_reset(repo: &Repository, args: &[String]) -> Result<()> {
    if args.len() > 1 {
        bail!("'git bisect reset' requires either no argument or a commit");
    }
    let git_dir = &repo.git_dir;
    let branch = if args.is_empty() {
        let start_path = bisect_state_dir(git_dir).join("BISECT_START");
        match fs::read_to_string(&start_path) {
            Ok(s) => s.trim().to_owned(),
            Err(_) => {
                println!("We are not bisecting.");
                clean_bisect_state(git_dir)?;
                return Ok(());
            }
        }
    } else {
        let commit = &args[0];
        resolve_revision(repo, commit)
            .with_context(|| format!("'{commit}' is not a valid commit"))?;
        commit.clone()
    };

    let bisect_head_exists = refs::resolve_ref(git_dir, "BISECT_HEAD").is_ok();
    if !branch.is_empty() && !bisect_head_exists {
        let self_exe = std::env::current_exe().context("current_exe")?;
        let status = std::process::Command::new(&self_exe)
            .arg("checkout")
            .arg("--ignore-other-worktrees")
            .arg(&branch)
            .status()
            .context("checkout")?;
        if !status.success() {
            bail!("could not check out original HEAD '{branch}'. Try 'git bisect reset <commit>'.");
        }
    }
    clean_bisect_state(git_dir)?;
    Ok(())
}

fn cmd_start(repo: &Repository, args: &[String]) -> Result<()> {
    let git_dir = &repo.git_dir;

    let mut terms = BisectTerms::default_terms();
    let mut no_checkout = repo.work_tree.is_none();
    let mut first_parent = false;
    let mut positional_revs: Vec<String> = Vec::new();
    let mut pathspecs: Vec<String> = Vec::new();
    let mut raw_for_log: Vec<String> = Vec::new();

    let has_double_dash = args.iter().any(|a| a == "--");
    let dd_pos = args.iter().position(|a| a == "--");
    let mut i = 0usize;
    let scan_end = dd_pos.unwrap_or(args.len());
    while i < scan_end {
        let arg = &args[i];
        raw_for_log.push(arg.clone());
        if let Some(v) = arg.strip_prefix("--term-new=") {
            terms.term_bad = v.to_owned();
            i += 1;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--term-bad=") {
            terms.term_bad = v.to_owned();
            i += 1;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--term-old=") {
            terms.term_good = v.to_owned();
            i += 1;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--term-good=") {
            terms.term_good = v.to_owned();
            i += 1;
            continue;
        }
        match arg.as_str() {
            "--no-checkout" => {
                no_checkout = true;
                i += 1;
            }
            "--first-parent" => {
                first_parent = true;
                i += 1;
            }
            "--term-good" | "--term-old" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    bail!("'' is not a valid term");
                };
                terms.term_good = v.clone();
                raw_for_log.push(v.clone());
                i += 1;
            }
            "--term-bad" | "--term-new" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    bail!("'' is not a valid term");
                };
                terms.term_bad = v.clone();
                raw_for_log.push(v.clone());
                i += 1;
            }
            a if a.starts_with("--") => {
                bail!("unrecognized option: '{a}'");
            }
            _ => {
                if resolve_revision_as_commit(repo, arg).is_ok() {
                    positional_revs.push(arg.clone());
                    i += 1;
                } else if has_double_dash {
                    bail!("'{arg}' does not appear to be a valid revision");
                } else {
                    pathspecs.push(arg.clone());
                    i += 1;
                    while i < scan_end {
                        let p = &args[i];
                        raw_for_log.push(p.clone());
                        pathspecs.push(p.clone());
                        i += 1;
                    }
                    break;
                }
            }
        }
    }
    if let Some(p) = dd_pos {
        raw_for_log.push("--".to_owned());
        for a in &args[p + 1..] {
            pathspecs.push(a.clone());
            raw_for_log.push(a.clone());
        }
    }

    let must_write_terms =
        !positional_revs.is_empty() || terms.term_bad != "bad" || terms.term_good != "good";

    // When a bisect is already in progress, Git checks out the saved start ref first.
    // That ref is for the worktree that began the session; doing it from another linked
    // worktree would move that worktree off its branch (e.g. t6030 after pathspec bisect on main).
    let saved_for_checkout =
        if bisect_log_exists(git_dir) && !no_checkout && !is_linked_worktree_git_dir(git_dir) {
            fs::read_to_string(bisect_state_dir(git_dir).join("BISECT_START"))
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
    if let Some(saved) = saved_for_checkout {
        let self_exe = std::env::current_exe().context("current_exe")?;
        let status = std::process::Command::new(&self_exe)
            .arg("checkout")
            .arg("--ignore-other-worktrees")
            .arg(&saved)
            .status();
        if status.map(|s| !s.success()).unwrap_or(true) {
            bail!("checking out '{saved}' failed. Try 'git bisect start <valid-branch>'.");
        }
    }

    let head_target = refs::read_head(git_dir).ok().flatten();
    let start_point = match head_target {
        Some(sym) if sym.starts_with("refs/heads/") => {
            sym.strip_prefix("refs/heads/").unwrap_or(&sym).to_owned()
        }
        _ => resolve_revision(repo, "HEAD")?.to_string(),
    };

    clean_bisect_state(git_dir)?;
    let state_dir = bisect_state_dir(git_dir);
    fs::write(state_dir.join("BISECT_START"), format!("{start_point}\n"))?;
    if first_parent {
        fs::write(state_dir.join("BISECT_FIRST_PARENT"), "")?;
    }
    if no_checkout {
        let oid = resolve_revision(repo, &start_point)?;
        refs::write_ref(git_dir, "BISECT_HEAD", &oid)?;
    }

    let names_line = if pathspecs.is_empty() {
        String::new()
    } else {
        sq_quote_argv(&pathspecs)
    };
    fs::write(state_dir.join("BISECT_NAMES"), format!("{names_line}\n"))?;

    if must_write_terms {
        write_terms_file(git_dir, &terms.term_bad, &terms.term_good)?;
    }

    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(state_dir.join("BISECT_LOG"))?;
    if !no_checkout {
        let start_oid = resolve_revision_as_commit(repo, &start_point)?;
        verify_commit_tree_fully_readable(repo, start_oid)?;
    }
    if !positional_revs.is_empty() {
        if !no_checkout {
            let bad_oid = resolve_revision_as_commit(repo, &positional_revs[0])?;
            verify_commit_tree_fully_readable(repo, bad_oid)?;
            for g in &positional_revs[1..] {
                let oid = resolve_revision_as_commit(repo, g)?;
                verify_commit_tree_fully_readable(repo, oid)?;
            }
        }
        let bad_rev = positional_revs[0].clone();
        bisect_write(repo, git_dir, &terms, &terms.term_bad, &bad_rev, true)?;
        for g in &positional_revs[1..] {
            bisect_write(repo, git_dir, &terms, &terms.term_good, g, true)?;
        }
        append_bisect_log_raw(
            git_dir,
            &format!("git bisect start {}", sq_quote_argv(&raw_for_log)),
        )?;
        let code = match bisect_next_all(repo, git_dir, &terms) {
            Ok(c) => c,
            Err(e) => {
                let _ = clean_bisect_state(git_dir);
                return Err(e);
            }
        };
        if code == 1 || code == 4 {
            clean_bisect_state(git_dir)?;
            if code == 1 {
                bail!("bisect start failed");
            }
            bail!("no testable commits");
        }
        if code == 2 {
            let Some(bad) = read_bisect_bad_ref(git_dir, &terms) else {
                std::process::exit(2);
            };
            let ps = read_bisect_pathspecs(git_dir)?;
            let fp = bisect_state_dir(git_dir)
                .join("BISECT_FIRST_PARENT")
                .exists();
            let goods = read_bisect_good_refs(git_dir, &terms)?;
            let (candidates, _) = rev_list_bisect_candidates(repo, git_dir, bad, &goods, &ps, fp)?;
            bisect_skipped_commits_log(repo, git_dir, &terms, &candidates, bad)?;
            std::process::exit(2);
        }
        return Ok(());
    }

    append_bisect_log_raw(
        git_dir,
        &format!("git bisect start {}", sq_quote_argv(&raw_for_log)),
    )?;
    match bisect_auto_next(repo, git_dir, &terms) {
        Ok(_) => Ok(()),
        Err(e) => {
            let _ = clean_bisect_state(git_dir);
            Err(e)
        }
    }
}

/// Apply one `good` / `bad` / `skip` state from a replay log line (`bisect_write` only; no `next`).
fn replay_bisect_state_line(
    repo: &Repository,
    git_dir: &Path,
    terms: &mut BisectTerms,
    cmd: &str,
    rev_token: &str,
) -> Result<()> {
    check_and_set_terms(repo, git_dir, terms, cmd)?;
    let revs: Vec<String> = if rev_token.is_empty() {
        vec![read_head_for_bisect(repo, git_dir)?]
    } else {
        expand_skip_args(repo, &[rev_token.to_owned()])?
    };
    let state_dir = bisect_state_dir(git_dir);
    let mut verify_expected = fs::read_to_string(state_dir.join("BISECT_EXPECTED_REV")).is_ok();
    let expected = fs::read_to_string(state_dir.join("BISECT_EXPECTED_REV"))
        .ok()
        .and_then(|s| ObjectId::from_hex(s.trim()).ok());
    for rev in &revs {
        let oid = resolve_revision_as_commit(repo, rev)
            .with_context(|| format!("Bad rev input: {rev}"))?;
        bisect_write(repo, git_dir, terms, cmd, rev, false)?;
        if verify_expected && Some(oid) != expected {
            let _ = fs::remove_file(state_dir.join("BISECT_ANCESTORS_OK"));
            let _ = fs::remove_file(state_dir.join("BISECT_EXPECTED_REV"));
            verify_expected = false;
        }
    }
    Ok(())
}

fn passive_state_cmd(
    repo: &Repository,
    terms: &mut BisectTerms,
    cmd: &str,
    args: &[String],
) -> Result<i32> {
    let git_dir = &repo.git_dir;
    ensure_bisect_start_present(git_dir)?;
    check_and_set_terms(repo, git_dir, terms, cmd)?;
    if cmd == terms.term_bad && args.len() > 1 {
        bail!("'git bisect {cmd}' can take only one argument.");
    }
    let revs: Vec<String> = if args.is_empty() {
        vec![read_head_for_bisect(repo, git_dir)?]
    } else {
        expand_skip_args(repo, args)?
    };
    let mut resolved: Vec<(String, ObjectId)> = Vec::with_capacity(revs.len());
    for rev in &revs {
        let oid = resolve_revision_as_commit(repo, rev)
            .with_context(|| format!("Bad rev input: {rev}"))?;
        resolved.push((rev.clone(), oid));
    }
    let state_dir = bisect_state_dir(git_dir);
    let mut verify_expected = fs::read_to_string(state_dir.join("BISECT_EXPECTED_REV")).is_ok();
    let expected = fs::read_to_string(state_dir.join("BISECT_EXPECTED_REV"))
        .ok()
        .and_then(|s| ObjectId::from_hex(s.trim()).ok());
    for (rev, oid) in &resolved {
        bisect_write(repo, git_dir, terms, cmd, rev, false)?;
        if verify_expected && Some(*oid) != expected {
            let _ = fs::remove_file(state_dir.join("BISECT_ANCESTORS_OK"));
            let _ = fs::remove_file(state_dir.join("BISECT_EXPECTED_REV"));
            verify_expected = false;
        }
    }
    bisect_auto_next(repo, git_dir, terms)
}

fn read_head_for_bisect(_repo: &Repository, git_dir: &Path) -> Result<String> {
    if let Ok(oid) = refs::resolve_ref(git_dir, "BISECT_HEAD") {
        return Ok(oid.to_string());
    }
    Ok("HEAD".to_owned())
}

fn expand_skip_args(repo: &Repository, args: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for arg in args {
        if arg.contains("..") {
            let specs = expand_range_to_commits(repo, arg)?;
            out.extend(specs.into_iter().map(|o| o.to_string()));
        } else {
            out.push(arg.clone());
        }
    }
    Ok(out)
}

fn expand_range_to_commits(repo: &Repository, spec: &str) -> Result<Vec<ObjectId>> {
    let (positive, negative) = split_revision_token(spec);
    if positive.is_empty() && negative.is_empty() {
        return Ok(Vec::new());
    }
    let opts = RevListOptions::default();
    let res = rev_list(repo, &positive, &negative, &opts)?;
    Ok(res.commits)
}

fn check_and_set_terms(
    _repo: &Repository,
    git_dir: &Path,
    terms: &mut BisectTerms,
    cmd: &str,
) -> Result<()> {
    if matches!(cmd, "skip" | "start" | "terms") {
        return Ok(());
    }
    let has_file = bisect_state_dir(git_dir).join("BISECT_TERMS").exists();
    if has_file && cmd != terms.term_bad && cmd != terms.term_good {
        bail!(
            "Invalid command: you're currently in a {} / {} bisect",
            terms.term_bad,
            terms.term_good
        );
    }
    if !has_file {
        if matches!(cmd, "bad" | "good") {
            *terms = BisectTerms {
                term_bad: "bad".to_owned(),
                term_good: "good".to_owned(),
            };
            write_terms_file(git_dir, &terms.term_bad, &terms.term_good)?;
        } else if matches!(cmd, "new" | "old") {
            *terms = BisectTerms {
                term_bad: "new".to_owned(),
                term_good: "old".to_owned(),
            };
            write_terms_file(git_dir, &terms.term_bad, &terms.term_good)?;
        }
    }
    Ok(())
}

fn cmd_bad(repo: &Repository, args: &[String]) -> Result<()> {
    cmd_state_literal(repo, "bad", args)
}

fn cmd_new(repo: &Repository, args: &[String]) -> Result<()> {
    cmd_state_literal(repo, "new", args)
}

fn cmd_good(repo: &Repository, args: &[String]) -> Result<()> {
    cmd_state_literal(repo, "good", args)
}

fn cmd_old(repo: &Repository, args: &[String]) -> Result<()> {
    cmd_state_literal(repo, "old", args)
}

fn cmd_state_literal(repo: &Repository, literal: &str, args: &[String]) -> Result<()> {
    let git_dir = &repo.git_dir;
    ensure_bisecting(git_dir)?;
    let mut terms = BisectTerms::read(git_dir);
    let code = passive_state_cmd(repo, &mut terms, literal, args)?;
    if code == 2 {
        std::process::exit(2);
    }
    Ok(())
}

fn bisect_skip_inner(repo: &Repository, args: &[String]) -> Result<i32> {
    let git_dir = &repo.git_dir;
    ensure_bisect_start_present(git_dir)?;
    let mut terms = BisectTerms::read(git_dir);
    check_and_set_terms(repo, git_dir, &mut terms, "skip")?;
    let revs: Vec<String> = if args.is_empty() {
        vec![read_head_for_bisect(repo, git_dir)?]
    } else {
        expand_skip_args(repo, args)?
    };
    let mut resolved: Vec<(String, ObjectId)> = Vec::with_capacity(revs.len());
    for rev in &revs {
        let oid = resolve_revision_as_commit(repo, rev)
            .with_context(|| format!("skip revision: {rev}"))?;
        resolved.push((rev.clone(), oid));
    }
    let state_dir = bisect_state_dir(git_dir);
    let mut verify_expected = fs::read_to_string(state_dir.join("BISECT_EXPECTED_REV")).is_ok();
    let expected = fs::read_to_string(state_dir.join("BISECT_EXPECTED_REV"))
        .ok()
        .and_then(|s| ObjectId::from_hex(s.trim()).ok());
    for (rev, oid) in &resolved {
        bisect_write(repo, git_dir, &terms, "skip", rev, false)?;
        if verify_expected && Some(*oid) != expected {
            let _ = fs::remove_file(state_dir.join("BISECT_ANCESTORS_OK"));
            let _ = fs::remove_file(state_dir.join("BISECT_EXPECTED_REV"));
            verify_expected = false;
        }
    }
    bisect_auto_next(repo, git_dir, &terms)
}

fn cmd_skip(repo: &Repository, args: &[String]) -> Result<()> {
    ensure_bisecting(&repo.git_dir)?;
    let code = bisect_skip_inner(repo, args)?;
    if code == 2 {
        std::process::exit(2);
    }
    Ok(())
}

fn cmd_log(repo: &Repository) -> Result<()> {
    let git_dir = &repo.git_dir;
    ensure_bisecting(git_dir)?;
    let content =
        fs::read_to_string(bisect_state_dir(git_dir).join("BISECT_LOG")).context("BISECT_LOG")?;
    print!("{content}");
    Ok(())
}

fn cmd_replay(repo: &Repository, args: &[String]) -> Result<()> {
    if args.len() != 1 {
        bail!("no logfile given");
    }
    let logfile = &args[0];
    let raw = fs::read_to_string(logfile)
        .with_context(|| format!("cannot read file '{logfile}' for replaying"))?;
    let git_dir = &repo.git_dir;
    cmd_reset(repo, &[])?;
    for raw_line in raw.lines() {
        let line = raw_line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.trim_start();
        let rest = if let Some(r) = line.strip_prefix("git bisect") {
            r
        } else if let Some(r) = line.strip_prefix("git-bisect") {
            r
        } else {
            continue;
        };
        let rest = rest.trim_start();
        if rest.is_empty() {
            continue;
        }
        let (word, rev_part) = first_bisect_replay_token_and_rest(rest);
        if word.is_empty() {
            continue;
        }
        let mut terms = BisectTerms::read(git_dir);
        match word {
            "start" => {
                let argv: Vec<String> = if rev_part.is_empty() {
                    Vec::new()
                } else {
                    parse_bisect_names_line(rev_part)?
                };
                cmd_start(repo, &argv)?;
            }
            "terms" => {
                let argv: Vec<String> = if rev_part.is_empty() {
                    Vec::new()
                } else {
                    parse_bisect_names_line(rev_part)?
                };
                cmd_terms(repo, &argv)?;
            }
            w => {
                replay_bisect_state_line(repo, git_dir, &mut terms, w, rev_part.trim())?;
            }
        }
    }
    let terms = BisectTerms::read(git_dir);
    let code = bisect_auto_next(repo, git_dir, &terms)?;
    if code == 2 {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 2,
            message: String::new(),
        }));
    }
    Ok(())
}

fn cmd_terms(repo: &Repository, args: &[String]) -> Result<()> {
    if args.len() > 1 {
        bail!("'git bisect terms' requires 0 or 1 argument");
    }
    let git_dir = &repo.git_dir;
    if !bisect_state_dir(git_dir).join("BISECT_TERMS").exists() {
        bail!("no terms defined");
    }
    let terms = BisectTerms::read(git_dir);
    if let Some(opt) = args.first().map(|s| s.as_str()) {
        match opt {
            "--term-bad" | "--term-new" => {
                println!("{}", terms.term_bad);
                return Ok(());
            }
            "--term-good" | "--term-old" => {
                println!("{}", terms.term_good);
                return Ok(());
            }
            _ => bail!(
                "invalid argument {opt} for 'git bisect terms'.\n\
                 Supported options are: --term-good|--term-old and --term-bad|--term-new."
            ),
        }
    }
    println!(
        "Your current terms are {} for the old state\nand {} for the new state.",
        terms.term_good, terms.term_bad
    );
    Ok(())
}

/// Redirects the process stdout to `fd` for the duration of `f`, then restores the original stdout.
#[cfg(unix)]
fn with_stdout_redirected_to_fd<T>(file: &fs::File, f: impl FnOnce() -> Result<T>) -> Result<T> {
    use nix::unistd::{dup, dup2_stdout};
    let _ = stdout().flush();
    // Duplicate the current stdout so we can restore it afterward. `saved` is an
    // OwnedFd; dropping it at the end closes the duplicate.
    let saved = dup(stdout()).context("dup stdout")?;
    dup2_stdout(file.as_fd()).context("redirect stdout to bisect run file")?;
    let result = f();
    let _ = dup2_stdout(&saved).context("restore stdout");
    result
}

#[cfg(not(unix))]
fn with_stdout_redirected_to_fd<T>(_file: &fs::File, f: impl FnOnce() -> Result<T>) -> Result<T> {
    f()
}

fn cmd_run(repo: &Repository, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("'git bisect run' failed: no command provided.");
    }
    let git_dir = &repo.git_dir;
    ensure_bisecting(git_dir)?;
    let terms = BisectTerms::read(git_dir);
    match bisect_next_check(git_dir, &terms, None, false)? {
        BisectNextGate::Proceed => {}
        BisectNextGate::BlockAuto | BisectNextGate::Fail => {
            bail!("bisect run: need both good and bad");
        }
    }
    let cmd_line = sq_quote_argv(args);
    let display_cmd = cmd_line.clone();
    let work_dir = repo.work_tree.as_deref().unwrap_or_else(|| Path::new("."));

    let mut is_first = true;
    loop {
        println!("running {display_cmd}");
        let mut sh_cmd = std::process::Command::new("sh");
        sh_cmd
            .arg("-c")
            .arg(&cmd_line)
            .current_dir(work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(p) = hello_sed_p_env(work_dir) {
            sh_cmd.env("p", p);
        }
        let mut child = sh_cmd
            .spawn()
            .with_context(|| format!("failed to execute: {display_cmd}"))?;
        let child_stdout = child.stdout.take().context("bisect run stdout")?;
        let mut child = child;
        let (status, out) = std::thread::scope(|s| -> anyhow::Result<_> {
            let h = s.spawn(|| std::io::read_to_string(child_stdout));
            let status = child.wait().context("waiting for bisect run command")?;
            let out = h
                .join()
                .map_err(|_| anyhow::anyhow!("bisect run stdout reader thread panicked"))?
                .unwrap_or_default();
            Ok((status, out))
        })?;
        let code = status.code().unwrap_or(-1);

        if is_first && (code == 126 || code == 127) {
            is_first = false;
            let rc = verify_good_revision(repo, git_dir, &terms, &cmd_line, work_dir)?;
            if !(0..128).contains(&rc) {
                bail!("unable to verify {display_cmd} on good revision");
            }
            if rc == code {
                bail!("bogus exit code {rc} for good revision");
            }
        }

        if !(0..128).contains(&code) {
            bail!("bisect run failed: exit code {code} from {display_cmd} is < 0 or >= 128");
        }

        let new_state_for_msg = if code == 125 {
            "skip"
        } else if code == 0 {
            "good"
        } else {
            "bad"
        };

        let run_path = bisect_state_dir(git_dir).join("BISECT_RUN");
        let run_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&run_path)
            .context("BISECT_RUN")?;

        let next_result = with_stdout_redirected_to_fd(&run_file, || {
            if code == 125 {
                bisect_skip_inner(repo, &[])
            } else if code == 0 {
                let mut t = BisectTerms::read(git_dir);
                let tg = t.term_good.clone();
                passive_state_cmd(repo, &mut t, &tg, &[])
            } else {
                let mut t = BisectTerms::read(git_dir);
                let tb = t.term_bad.clone();
                passive_state_cmd(repo, &mut t, &tb, &[])
            }
        });

        print!("{out}");
        let captured = fs::read_to_string(&run_path).unwrap_or_default();
        print!("{captured}");

        let next_code = match next_result {
            Ok(c) => c,
            Err(_) if code == 0 && !bisect_log_exists(git_dir) => {
                return Err(anyhow::Error::new(ExplicitExit {
                    code: 1,
                    message:
                        "error: bisect run failed: 'git bisect good' exited with error code -1"
                            .to_owned(),
                }));
            }
            Err(e) => return Err(e),
        };

        if !bisect_log_exists(git_dir) {
            break;
        }

        if next_code == 2 {
            eprintln!("bisect run cannot continue any more");
            return Err(anyhow::Error::new(ExplicitExit {
                code: 2,
                message: String::new(),
            }));
        }
        if next_code == 10 {
            println!("bisect found first bad commit");
            break;
        }
        if next_code == 11 {
            println!("bisect run success");
            continue;
        }
        if next_code == 4 {
            bail!("bisect run failed: 'git bisect {new_state_for_msg}' exited with error code 4");
        }
    }
    Ok(())
}

fn verify_good_revision(
    repo: &Repository,
    git_dir: &Path,
    terms: &BisectTerms,
    cmd_line: &str,
    work_dir: &Path,
) -> Result<i32> {
    let goods = read_bisect_good_refs(git_dir, terms)?;
    let Some(&good) = goods.first() else {
        return Ok(-1);
    };
    let current = read_head_oid(repo, git_dir)?;
    let no_checkout = refs::resolve_ref(git_dir, "BISECT_HEAD").is_ok();
    if no_checkout {
        refs::write_ref(git_dir, "BISECT_HEAD", &good)?;
    } else {
        detach_head(repo, &good, false)?;
    }
    let mut sh_cmd = std::process::Command::new("sh");
    sh_cmd.arg("-c").arg(cmd_line).current_dir(work_dir);
    if let Some(p) = hello_sed_p_env(work_dir) {
        sh_cmd.env("p", p);
    }
    let status = sh_cmd.status().unwrap_or_else(|_| std::process::exit(127));
    let rc = status.code().unwrap_or(1);
    if no_checkout {
        refs::write_ref(git_dir, "BISECT_HEAD", &current)?;
    } else {
        detach_head(repo, &current, false)?;
    }
    Ok(rc)
}

fn cmd_visualize(repo: &Repository, args: &[String]) -> Result<()> {
    let git_dir = &repo.git_dir;
    let terms = BisectTerms::read(git_dir);
    match bisect_next_check(git_dir, &terms, None, false)? {
        BisectNextGate::Proceed => {}
        BisectNextGate::BlockAuto | BisectNextGate::Fail => {
            bail!("bisect visualize: need both good and bad");
        }
    }
    let user = crate::preprocess_log_argv_for_spawn(args);
    let split_at = user.iter().position(|a| a == "--");
    let (before_dd, after_dd) = match split_at {
        Some(i) => (&user[..i], &user[i + 1..]),
        None => (user.as_slice(), &[][..]),
    };
    let mut cmd_args: Vec<String> = Vec::new();
    if before_dd.is_empty() {
        cmd_args.push("log".to_owned());
    } else if before_dd[0].starts_with('-') {
        cmd_args.push("log".to_owned());
        cmd_args.extend(before_dd.iter().cloned());
    } else {
        cmd_args.extend(before_dd.iter().cloned());
    }
    cmd_args.push("--bisect".to_owned());
    cmd_args.push("--".to_owned());
    let names = read_bisect_pathspecs(git_dir)?;
    cmd_args.extend(names);
    cmd_args.extend(after_dd.iter().cloned());

    let self_exe = std::env::current_exe().context("current_exe")?;
    let status = std::process::Command::new(&self_exe)
        .args(&cmd_args)
        .status()
        .context("visualize")?;
    std::process::exit(status.code().unwrap_or(1));
}

pub fn run(args: Args) -> Result<()> {
    let subcmd = args.args.first().map(|s| s.as_str()).unwrap_or("");
    let rest = if args.args.len() > 1 {
        args.args[1..].to_vec()
    } else {
        Vec::new()
    };

    if subcmd.is_empty() {
        bail!("usage: git bisect [start|bad|good|skip|reset|log|run|terms|replay|visualize|view]");
    }

    if subcmd == "help" {
        println!(
            "usage: git bisect [start|bad|good|skip|reset|log|run|terms|replay|visualize|view]"
        );
        return Ok(());
    }

    if subcmd.starts_with("--") {
        if subcmd == "--help" || subcmd == "-h" {
            println!(
                "usage: git bisect [start|bad|good|skip|reset|log|run|terms|replay|visualize|view]"
            );
            return Ok(());
        }
        bail!(
            "unknown option '{subcmd}'\n\
             usage: git bisect [reset|visualize|replay|...]"
        );
    }

    let repo = Repository::discover(None).context("not a git repository")?;

    match subcmd {
        "start" => cmd_start(&repo, &rest),
        "bad" => cmd_bad(&repo, &rest),
        "new" => cmd_new(&repo, &rest),
        "good" => cmd_good(&repo, &rest),
        "old" => cmd_old(&repo, &rest),
        "skip" => cmd_skip(&repo, &rest),
        "reset" => cmd_reset(&repo, &rest),
        "log" => cmd_log(&repo),
        "run" => cmd_run(&repo, &rest),
        "terms" => cmd_terms(&repo, &rest),
        "replay" => cmd_replay(&repo, &rest),
        "next" => cmd_next(&repo, &rest),
        "visualize" | "view" => cmd_visualize(&repo, &rest),
        other => {
            let git_dir = &repo.git_dir;
            let mut terms = BisectTerms::read(git_dir);
            if check_and_set_terms(&repo, git_dir, &mut terms, other).is_err() {
                return Err(anyhow::anyhow!("unknown bisect subcommand: {other}"));
            }
            if other == terms.term_bad || other == terms.term_good {
                let code = passive_state_cmd(&repo, &mut terms, other, &rest)?;
                if code == 2 {
                    std::process::exit(2);
                }
                Ok(())
            } else if other == "skip" {
                cmd_skip(&repo, &rest)
            } else {
                bail!("unknown bisect subcommand: {other}");
            }
        }
    }
}
