//! Shell helpers comparing `grit history` to system `git history` and scripted `git rebase`.

use std::path::Path;

use crate::bench_env::isolated_env_prefix;
use crate::binary::tool_version;
use crate::history_rewrite_fixture::HistoryRewriteMeta;
use crate::shell::shell_quote;

const BENCH_COMMITTER_DATE: &str = "1700000100 +0000";

/// Parse `(major, minor)` from `git version X.Y`.
#[must_use]
pub fn git_version_tuple(git_version: &str) -> Option<(u32, u32)> {
    let token = git_version.split_whitespace().nth(2)?;
    let mut parts = token.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

#[must_use]
pub fn git_supports_history(git_version: &str) -> bool {
    git_version_tuple(git_version).is_some_and(|(maj, min)| (maj, min) >= (2, 54))
}

/// Whether reword scenarios should compare against `git history reword`.
///
/// Git 2.54 introduced `git history`; drop, fixup, and squash arrived later (fixup/drop) or are
/// not implemented upstream (squash). Other rewrite scenarios always use scripted git baselines.
#[must_use]
pub fn git_reword_primary_is_history(git: &Path) -> bool {
    git_supports_history(&tool_version(git))
}

/// Drop/fixup/squash have no stable `git history` counterpart on typical system git builds.
#[must_use]
pub fn git_drop_primary_is_history(_git: &Path) -> bool {
    false
}

#[must_use]
pub fn git_fixup_primary_is_history(_git: &Path) -> bool {
    false
}

#[must_use]
pub fn git_squash_primary_is_history(_git: &Path) -> bool {
    false
}

fn history_env_prefix() -> String {
    format!(
        "{} GIT_COMMITTER_DATE='{BENCH_COMMITTER_DATE}'",
        isolated_env_prefix()
    )
}

fn grit_history_cmd(grit: &Path, parts: &[&str]) -> String {
    let grit_q = shell_quote(&grit.to_string_lossy());
    let mut cmd = format!("{grit_q} history");
    for p in parts {
        cmd.push(' ');
        cmd.push_str(&shell_quote(p));
    }
    cmd
}

fn git_history_cmd(git: &Path, parts: &[&str]) -> String {
    let git_q = shell_quote(&git.to_string_lossy());
    let mut cmd = format!("{git_q} history");
    for p in parts {
        cmd.push(' ');
        cmd.push_str(&shell_quote(p));
    }
    format!("{} {}", history_env_prefix(), cmd)
}

/// Grit and git argv (shell) for reword at the configured depth.
pub fn reword_scenario(
    grit: &Path,
    git: &Path,
    meta: &HistoryRewriteMeta,
    update_refs_head: bool,
) -> (Vec<String>, Vec<String>) {
    let msg = meta.bench_message.trim();
    let mut grit_parts = vec!["reword", &meta.reword, "-m", msg];
    let mut git_parts = vec!["reword", &meta.reword];
    if update_refs_head {
        grit_parts.push("--update-refs=head");
        git_parts.push("--update-refs=head");
    }
    let grit_cmd = grit_history_cmd(grit, &grit_parts);
    let git_cmd = format!(
        "GIT_EDITOR=./.grit-bench-git-editor.sh {}",
        git_history_cmd(git, &git_parts)
    );
    (
        vec!["-c".into(), format!("set -eu; {grit_cmd} >/dev/null")],
        vec!["-c".into(), format!("set -eu; {git_cmd} >/dev/null")],
    )
}

pub fn reword_git_rebase_scenario(repo: &Path) -> Vec<String> {
    script_scenario(repo, ".grit-bench/git-reword-rebase.sh")
}

/// Grit and git argv for `rebase <upstream>` at the configured depth.
pub fn rebase_onto_scenario(
    grit: &Path,
    git: &Path,
    meta: &HistoryRewriteMeta,
) -> (Vec<String>, Vec<String>) {
    let grit_cmd = format!(
        "{} rebase {} >/dev/null",
        shell_quote(&grit.to_string_lossy()),
        shell_quote(&meta.rebase_upstream)
    );
    let git_cmd = format!(
        "{} {} rebase {} >/dev/null",
        history_env_prefix(),
        shell_quote(&git.to_string_lossy()),
        shell_quote(&meta.rebase_upstream)
    );
    (
        vec!["-c".into(), format!("set -eu; {grit_cmd}")],
        vec!["-c".into(), format!("set -eu; {git_cmd}")],
    )
}

pub fn drop_scenario(
    grit: &Path,
    git: &Path,
    meta: &HistoryRewriteMeta,
) -> (Vec<String>, Vec<String>) {
    let grit = grit_history_cmd(grit, &["drop", &meta.drop]);
    let git = git_history_cmd(git, &["drop", &meta.drop]);
    (
        vec!["-c".into(), format!("set -eu; {grit} >/dev/null")],
        vec!["-c".into(), format!("set -eu; {git} >/dev/null")],
    )
}

pub fn drop_git_rebase_scenario(repo: &Path) -> Vec<String> {
    script_scenario(repo, ".grit-bench/git-drop-rebase.sh")
}

pub fn fixup_scenario(
    grit: &Path,
    git: &Path,
    meta: &HistoryRewriteMeta,
) -> (Vec<String>, Vec<String>) {
    let grit = grit_history_cmd(grit, &["fixup", &meta.fixup, "--update-refs=head"]);
    let git = git_history_cmd(git, &["fixup", &meta.fixup, "--update-refs=head"]);
    (
        vec!["-c".into(), format!("set -eu; {grit} >/dev/null")],
        vec!["-c".into(), format!("set -eu; {git} >/dev/null")],
    )
}

pub fn squash_scenario(
    grit: &Path,
    git: &Path,
    meta: &HistoryRewriteMeta,
) -> (Vec<String>, Vec<String>) {
    let grit = grit_history_cmd(
        grit,
        &[
            "squash",
            &meta.squash_range,
            "-m",
            meta.bench_message.trim(),
            "--update-refs=head",
        ],
    );
    let git = git_history_cmd(
        git,
        &[
            "squash",
            &meta.squash_range,
            "-m",
            meta.bench_message.trim(),
            "--update-refs=head",
        ],
    );
    (
        vec!["-c".into(), format!("set -eu; {grit} >/dev/null")],
        vec!["-c".into(), format!("set -eu; {git} >/dev/null")],
    )
}

pub fn squash_git_rebase_scenario(repo: &Path) -> Vec<String> {
    script_scenario(repo, ".grit-bench/git-squash-rebase.sh")
}

fn script_scenario(_repo: &Path, rel: &str) -> Vec<String> {
    vec![
        "-c".into(),
        format!("set -eu; {} ./{rel} >/dev/null", history_env_prefix(),),
    ]
}

pub fn fixup_git_rebase_scenario(repo: &Path) -> Vec<String> {
    script_scenario(repo, ".grit-bench/git-fixup-rebase.sh")
}
