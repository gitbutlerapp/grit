//! Upstream-style `git <cmd> -h` / `--help` synopsis printing (from vendored `Documentation/*.adoc`).
//!
//! Git prints a short synopsis for `-h` (usually exit **129** on stdout). `git rev-parse -h` is
//! special: synopsis on stderr plus a one-line pointer to `--parseopt -h`. `git submodule -h`
//! exits **0** (t7400).

use std::io::{self, Write};

mod upstream_help_builtin_synopsis {
    include!(concat!(env!("OUT_DIR"), "/upstream_help_synopsis.rs"));
}

/// Vendored synopsis string for `cmd`, if present in `upstream_help_synopsis.rs`.
///
/// A handful of commands (e.g. `imap-send`) are not part of the `--list-cmds=builtins`
/// union the generator iterates over, so they never appear in the generated table even
/// though `Documentation/git-<cmd>.adoc` has a SYNOPSIS block. Provide their adoc
/// SYNOPSIS here so their `-h` output still agrees with the docs byte-for-byte (t0450).
#[must_use]
pub(crate) fn synopsis_for_builtin(cmd: &str) -> Option<&'static str> {
    if let Some(syn) = upstream_help_builtin_synopsis::synopsis_for_builtin(cmd) {
        return Some(syn);
    }
    synopsis_for_non_builtin(cmd)
}

/// adoc SYNOPSIS for commands omitted from the generated `--list-cmds=builtins` table.
/// Text mirrors `Documentation/git-<cmd>.adoc` after the t0450 `adoc_to_synopsis` rules.
#[must_use]
fn synopsis_for_non_builtin(cmd: &str) -> Option<&'static str> {
    match cmd {
        "imap-send" => Some(concat!(
            "git imap-send [-v] [-q] [--[no-]curl] [(--folder|-f) <folder>]\n",
            "git imap-send --list",
        )),
        "instaweb" => Some(concat!(
            "git instaweb [--local] [--httpd=<httpd>] [--port=<port>]\n",
            "               [--browser=<browser>]\n",
            "git instaweb [--start] [--stop] [--restart]",
        )),
        _ => None,
    }
}

/// Print synopsis to stdout (no trailer), blank line, then exit — used by `parse_cmd_args` and stash helpers.
pub(crate) fn print_upstream_synopsis_stdout_and_exit(subcmd: &str, syn: &str, exit_code: u8) -> ! {
    let mut out = std::io::stdout();
    if let Err(e) = print_upstream_synopsis_to(subcmd, syn, &mut out) {
        eprintln!("failed to write help: {e}");
        std::process::exit(128);
    }
    std::process::exit(exit_code.into());
}

/// Split adoc synopsis into usage variants: each variant starts with a `git …` line; following
/// lines are continuations (AsciiDoc tabs) until the next `git …` line.
pub(crate) fn synopsis_variants_from_adoc(syn: &str) -> Vec<Vec<String>> {
    let mut variants: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in syn.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("git ") && !current.is_empty() {
            variants.push(core::mem::take(&mut current));
        }
        current.push(trimmed.to_owned());
    }
    if !current.is_empty() {
        variants.push(current);
    }
    variants
}

fn print_upstream_synopsis_to(subcmd: &str, syn: &str, out: &mut dyn Write) -> io::Result<()> {
    let pad = " ".repeat(format!("git {subcmd} ").len());
    let variants = synopsis_variants_from_adoc(syn);
    for (i, var) in variants.iter().enumerate() {
        let Some(first) = var.first() else {
            continue;
        };
        if i == 0 {
            writeln!(out, "usage: {first}")?;
        } else {
            writeln!(out, "   or: {first}")?;
        }
        for cont in var.iter().skip(1) {
            writeln!(out, "{pad}{cont}")?;
        }
    }
    writeln!(out)?;
    Ok(())
}

/// If `rest` is exactly `-h`, `--help`, or `--help-all`, print the upstream synopsis and exit.
/// Otherwise no-op.
///
/// `--help-all` matches the short synopsis (`-h`) and exit **129** (t1517). Long `--help` alone
/// exits **0** so POSIX `&&` chains keep working (t0450).
///
/// The synopsis text is the vendored `Documentation/*.adoc` SYNOPSIS, printed to **stdout** so the
/// `-h` output and the `*.adoc` SYNOPSIS agree byte-for-byte (t0450). `git rev-parse -h` therefore
/// prints `usage: git rev-parse [<options>] <arg>...` (the adoc synopsis), not the historical
/// `--parseopt`/`--sq-quote` three-line form.
pub(crate) fn try_print_upstream_help_and_exit(subcmd: &str, rest: &[String]) {
    if rest.len() != 1 {
        return;
    }
    let flag = rest[0].as_str();
    if flag != "-h" && flag != "--help" && flag != "--help-all" {
        return;
    }
    let long_help = flag == "--help";

    let exit_code: i32 = if subcmd == "submodule" || long_help {
        0
    } else {
        129
    };

    let Some(syn) = synopsis_for_builtin(subcmd) else {
        return;
    };
    let mut out = std::io::stdout();
    let result = print_upstream_synopsis_to(subcmd, syn, &mut out);

    if let Err(e) = result {
        eprintln!("failed to write help: {e}");
        std::process::exit(128);
    }
    std::process::exit(exit_code);
}
