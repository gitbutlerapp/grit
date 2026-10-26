//! Map common Git-habit argv mistakes to grit spellings before raw clap output.
//!
//! Clap's generic ``tip: to pass X as a value, use '-- X'`` advice is wrong for
//! almost every case here (for example `grit checkout -- -b feature` would try to
//! switch to a branch named `-b`). When we recognize a pattern, emit a `hint:` and
//! show usage under the subcommand name the user typed (`checkout`, not `switch`).

use std::ffi::OsString;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Error;
use clap::error::ErrorKind;

use crate::error_kind::ErrorKind as CliErrorKind;
use crate::json_error::{StructuredCliError, StructuredJsonError};

static ARGV: OnceLock<Vec<OsString>> = OnceLock::new();

/// Remember argv for the rest of the process (runtime hints after successful parse).
pub fn capture_argv(argv: Vec<OsString>) {
    let _ = ARGV.set(argv);
}

/// Original process argv, or empty when not captured yet.
pub fn argv() -> &'static [OsString] {
    ARGV.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Hint when `checkout`/`co` was used but the name is a path, not a branch.
pub fn checkout_restore_hint(name: &str) -> Option<String> {
    let display = user_switch_display(argv())?;
    if tokens_after_separator(argv()).any(|t| t == name) {
        return Some(format!(
            "restore worktree files with `grit restore {name}` (you ran `grit {display}`)"
        ));
    }
    if path_exists_in_worktree(name) {
        return Some(format!(
            "restore worktree files with `grit restore {name}` (you ran `grit {display}`)"
        ));
    }
    None
}

/// Turn a clap usage failure into a structured error when we recognize Git habits.
pub fn clap_failure(argv: &[OsString], err: &clap::Error) -> Option<Error> {
    let hint = migration_hint(argv, err)?;
    let human = clap_human_message(err, Some(hint.as_str()));
    Some(Error::new(StructuredCliError {
        payload: StructuredJsonError {
            hint: Some(hint.clone()),
            ..StructuredJsonError::new(human.clone(), CliErrorKind::Usage)
        },
        human,
    }))
}

/// Usage and help trailer lines after a mapped clap failure (human mode only).
pub fn eprint_clap_trailer(argv: &[OsString], err: &clap::Error) {
    for line in clap_usage_lines(argv, err) {
        eprintln!("{line}");
    }
    for line in err.to_string().lines() {
        if line.trim_start().starts_with("For more information") {
            eprintln!("{line}");
        }
    }
}

fn migration_hint(argv: &[OsString], err: &clap::Error) -> Option<String> {
    match err.kind() {
        ErrorKind::UnknownArgument => {}
        ErrorKind::TooManyValues => {}
        _ => return None,
    }
    let bad = unexpected_argument(err)?;
    let sub = user_subcommand(argv)?;

    if matches!(sub.as_str(), "switch" | "checkout" | "co")
        && matches!(bad.as_str(), "-b" | "--branch")
    {
        let name = branch_name_after_git_create_flag(argv).unwrap_or_else(|| "<name>".to_owned());
        return Some(format!(
            "create a branch and switch with `grit switch -c {name}`"
        ));
    }

    if sub == "add" && matches!(bad.as_str(), "-A" | "-a" | "--all" | "--update") {
        return Some(
            "`grit add` with no paths stages everything in the repository (tracked and untracked)"
                .to_owned(),
        );
    }

    if sub == "init" && matches!(bad.as_str(), "-q" | "--quiet") {
        return Some("grit init has no quiet flag; output is already brief".to_owned());
    }

    if sub == "reset" {
        return reset_unstage_hint(argv, &bad);
    }

    None
}

fn reset_unstage_hint(argv: &[OsString], path: &str) -> Option<String> {
    if path.starts_with('-') {
        return None;
    }
    if !argv.iter().any(|a| a == "HEAD") {
        return None;
    }
    Some(format!(
        "unstage paths with `grit restore --staged {}`",
        shell_word(path)
    ))
}

fn clap_human_message(err: &clap::Error, hint: Option<&str>) -> String {
    let mut lines: Vec<String> = err
        .to_string()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    lines.retain(|line| !is_generic_pass_as_value_tip(line));
    if lines.is_empty() {
        return "invalid arguments".to_owned();
    }
    let first = lines[0]
        .strip_prefix("error:")
        .or_else(|| lines[0].strip_prefix("error: "))
        .unwrap_or(&lines[0])
        .trim()
        .to_owned();
    if hint.is_some() {
        first
    } else {
        lines.join("\n")
    }
}

pub(crate) fn clap_usage_lines(argv: &[OsString], err: &clap::Error) -> Vec<String> {
    err.to_string()
        .lines()
        .filter(|line| line.trim_start().starts_with("Usage:"))
        .map(|line| rewrite_usage_line(argv, line))
        .collect()
}

fn rewrite_usage_line(argv: &[OsString], line: &str) -> String {
    let Some(display) = user_switch_display(argv) else {
        return line.to_owned();
    };
    line.replace("Usage: grit switch", &format!("Usage: grit {display}"))
        .replace("Usage: grit co", &format!("Usage: grit {display}"))
        .replace("Usage: grit checkout", &format!("Usage: grit {display}"))
}

fn is_generic_pass_as_value_tip(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("tip:") && trimmed.contains("to pass") && trimmed.contains("use '--")
}

fn unexpected_argument(err: &clap::Error) -> Option<String> {
    let rendered = err.to_string();
    let first = rendered.lines().next()?;
    let rest = first.strip_prefix("error:")?.trim();
    let rest = rest.strip_prefix("unexpected argument ")?;
    let quoted = rest.strip_prefix('\'')?;
    let (token, _) = quoted.split_once('\'')?;
    Some(token.to_owned())
}

fn user_subcommand(argv: &[OsString]) -> Option<String> {
    let i = crate::diff_args::subcommand_index(argv)?;
    argv[i].to_str().map(str::to_owned)
}

fn user_switch_display(argv: &[OsString]) -> Option<&'static str> {
    for arg in argv.iter().skip(1) {
        if arg == "checkout" {
            return Some("checkout");
        }
        if arg == "co" {
            return Some("co");
        }
    }
    None
}

fn branch_name_after_git_create_flag(argv: &[OsString]) -> Option<String> {
    for (i, arg) in argv.iter().enumerate() {
        if arg == "-b" || arg == "--branch" {
            return argv
                .get(i + 1)
                .and_then(|arg| arg.to_str())
                .map(str::to_owned);
        }
    }
    None
}

fn tokens_after_separator(argv: &[OsString]) -> impl Iterator<Item = &str> {
    let pos = argv.iter().position(|a| a == "--");
    pos.map(|i| argv[i + 1..].iter().filter_map(|arg| arg.to_str()))
        .into_iter()
        .flatten()
}

fn path_exists_in_worktree(name: &str) -> bool {
    Path::new(name).exists()
}

fn shell_word(path: &str) -> String {
    let plain = !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/+@%,=:".contains(c));
    if plain {
        path.to_owned()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_error::structured_json_payload;
    use clap::Parser;

    #[derive(Debug, Parser)]
    #[command(name = "grit")]
    struct Mini {
        #[arg(long, global = true, value_name = "EXPR")]
        filter: Option<String>,
        #[arg(long, global = true, value_name = "N")]
        threads: Option<usize>,
        #[command(subcommand)]
        cmd: MiniCmd,
    }

    #[derive(Debug, clap::Subcommand)]
    enum MiniCmd {
        #[command(alias = "checkout", alias = "co")]
        Switch {
            #[arg(short = 'c', long = "create")]
            create: bool,
            branch: Option<String>,
        },
        Add {
            paths: Vec<String>,
        },
        Reset {
            rev: Option<String>,
        },
        Init {
            path: Option<String>,
        },
    }

    fn parse_err(argv: &[&str]) -> clap::Error {
        Mini::try_parse_from(argv).unwrap_err()
    }

    fn hint_for(argv: &[&str]) -> String {
        let owned: Vec<OsString> = argv.iter().map(OsString::from).collect();
        let err = parse_err(argv);
        let structured = clap_failure(&owned, &err).expect("hint");
        structured_json_payload(&structured)
            .expect("payload")
            .hint
            .expect("hint text")
    }

    #[test]
    fn checkout_dash_b_points_at_switch_create() {
        assert!(hint_for(&["grit", "checkout", "-b", "feature"]).contains("grit switch -c feature"));
    }

    #[test]
    fn global_filter_before_subcommand_still_matches_add_dash_a() {
        let hint = hint_for(&["grit", "--filter", ".paths", "add", "-A"]);
        assert!(hint.contains("no paths stages everything"));
    }

    #[test]
    fn global_threads_before_subcommand_still_matches_checkout_dash_b() {
        let hint = hint_for(&["grit", "--threads", "2", "checkout", "-b", "feature"]);
        assert!(hint.contains("grit switch -c feature"));
    }

    #[test]
    fn slash_or_dotted_missing_branch_does_not_get_restore_hint() {
        capture_argv(vec!["grit".into(), "checkout".into(), "feature/foo".into()]);
        assert!(checkout_restore_hint("feature/foo").is_none());
        capture_argv(vec!["grit".into(), "checkout".into(), "release-1.0".into()]);
        assert!(checkout_restore_hint("release-1.0").is_none());
    }

    #[test]
    fn add_dash_a_explains_default_staging() {
        let hint = hint_for(&["grit", "add", "-A"]);
        assert!(hint.contains("no paths stages everything"));
    }

    #[test]
    fn reset_head_path_points_at_restore_staged() {
        let hint = hint_for(&["grit", "reset", "HEAD", "n"]);
        assert!(hint.contains("grit restore --staged n"));
    }

    #[test]
    fn init_dash_q_notes_no_quiet_flag() {
        assert!(hint_for(&["grit", "init", "-q", "other"]).contains("no quiet flag"));
    }

    #[test]
    fn strips_generic_clap_pass_as_value_tip_from_message() {
        let err = parse_err(&["grit", "checkout", "-b", "feature"]);
        let msg = clap_human_message(&err, Some("hint"));
        assert!(!msg.contains("tip:"));
        assert!(msg.contains("unexpected argument '-b'"));
    }

    #[test]
    fn checkout_restore_hint_after_double_dash() {
        capture_argv(vec![
            "grit".into(),
            "checkout".into(),
            "--".into(),
            "a".into(),
        ]);
        let hint = checkout_restore_hint("a").expect("hint");
        assert!(hint.contains("grit restore a"));
    }
}
