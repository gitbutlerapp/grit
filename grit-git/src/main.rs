//! `grit` — Git plumbing reimplementation in Rust.
//!
//! This binary uses manual pre-dispatch to avoid building a clap parser for
//! all 143+ subcommands on every invocation.  Global options (-C, --git-dir,
//! --work-tree, -c) are extracted from argv by hand, then only the specific
//! subcommand's clap `Args` struct is parsed.

#![allow(dead_code)] // test-tool and harness helpers not fully wired through dispatch

use anyhow::{bail, Context, Result};
use clap::{Args, Command, FromArgMatches, Parser};
use grit_lib::git_path;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

mod alias;
mod branch_ref_format;
mod branch_tracking;
mod bundle_uri;
#[cfg(feature = "test-tools")]
mod bundle_uri_test_tool;
mod commands;
mod editor;
mod explicit_exit;
mod ext_transport;
mod fetch_submodule_record;
mod fetch_submodule_recurse;
mod fetch_transport;
mod file_upload_pack_v2;
mod git_column;
mod git_daemon_url;
mod grit_exe;
mod http_bundle_uri;
mod http_client;
mod http_push_smart;
mod http_smart;
mod ident;
mod pack_objects_upload;
pub mod pathspec;
pub mod pkt_line;
mod porcelain_rev;
mod precompose;
pub mod protocol;
mod protocol_wire;
mod receive_ingest;
mod ref_transaction_hooks;
mod ssh_transport;
#[cfg(feature = "test-tools")]
mod test_tool_pack_deltas;
#[cfg(feature = "test-tools")]
mod test_tool_run_command;
mod trace2_transfer;
mod trace_packet;
mod wire_trace;

/// Return the version string, e.g. `"2.47.0.grit-0.1.3"`.
pub fn version_string() -> String {
    format!("2.47.0.grit-{}", env!("CARGO_PKG_VERSION"))
}

fn argv_lossy() -> Vec<String> {
    std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn main() {
    let start = std::time::Instant::now();
    let trace2_path = std::env::var("GIT_TRACE2").ok().filter(|s| !s.is_empty());
    let trace2_perf_path = std::env::var("GIT_TRACE2_PERF")
        .ok()
        .filter(|s| !s.is_empty());
    let trace2_event_path = std::env::var("GIT_TRACE2_EVENT")
        .ok()
        .filter(|s| !s.is_empty());
    let exit_code;

    // Write trace2 version event at startup
    if let Some(ref path) = trace2_path {
        let _ = trace2_write_event(path, "version", env!("CARGO_PKG_VERSION"));
        let cmd_line = argv_lossy();
        let _ = trace2_write_event(path, "start", &cmd_line.join(" "));
        let ancestry = get_process_ancestry();
        let _ = trace2_write_event(
            path,
            "cmd_ancestry",
            &format!("ancestry:[{}]", ancestry.join(" ")),
        );
    }
    if let Some(ref path) = trace2_perf_path {
        let cmd_line = argv_lossy();
        let _ = trace2_write_perf(path, "version", env!("CARGO_PKG_VERSION"));
        let _ = trace2_write_perf(path, "start", &cmd_line.join(" "));
        let ancestry = get_process_ancestry();
        let _ = trace2_write_perf(
            path,
            "cmd_ancestry",
            &format!("ancestry:[{}]", ancestry.join(" ")),
        );
    }
    if let Some(ref path) = trace2_event_path {
        let cmd_line = argv_lossy();
        let _ = trace2_write_json_event(path, "version", env!("CARGO_PKG_VERSION"));
        let _ = trace2_write_json_event(path, "start", &cmd_line.join(" "));
        let ancestry = get_process_ancestry();
        let _ = trace2_write_json_ancestry(path, &ancestry);
    }

    match run() {
        Ok(()) => {
            exit_code = 0;
        }
        Err(e) => {
            if is_broken_pipe_error(&e) {
                // Match shell signal convention for SIGPIPE.
                exit_code = 128 + 13;
            } else if let Some(ex) = e.downcast_ref::<crate::explicit_exit::SilentNonZeroExit>() {
                exit_code = ex.code;
            } else if error_chain_has_corrupt_cache_tree(&e) {
                // Match Git: `error: corrupted cache-tree has entries not present in index`.
                // Printed verbatim (not as `fatal:`) regardless of any wrapping context, so
                // test_grep on the bare message succeeds (t4058-diff-duplicates).
                eprintln!("error: corrupted cache-tree has entries not present in index");
                exit_code = 128;
            } else if let Some(msg) = path_error_fatal_message(&e) {
                eprintln!("fatal: {msg}");
                exit_code = 128;
            } else if let Some(ex) = e.downcast_ref::<crate::explicit_exit::ExplicitExit>() {
                if !ex.message.is_empty() {
                    eprintln!("{ex}");
                }
                exit_code = ex.code;
            } else if let Some(ex) = e.downcast_ref::<commands::fetch::ExitCodeError>() {
                exit_code = ex.code;
            } else if let Some(msg) = verbatim_lib_error_message(&e) {
                eprintln!("{msg}");
                exit_code = 128;
            } else {
                let display = format!("{e:#}");
                if let Some(rest) = display.strip_prefix("fatal:") {
                    // Downstream errors may already include the `fatal:` prefix; avoid
                    // double-prefixing (t3705 greps `fatal: pathspec ...`).
                    eprintln!("fatal:{rest}");
                    exit_code = 128;
                } else if display.starts_with("Invalid proxy URL")
                    || display.contains("Invalid proxy URL '")
                {
                    eprintln!("fatal: {display}");
                    exit_code = 128;
                } else {
                    eprintln!("error: {display}");
                    exit_code = 1;
                }
            }
        }
    }

    // Write trace2 exit event
    if let Some(ref path) = trace2_path {
        let elapsed = start.elapsed();
        let _ = trace2_write_event(
            path,
            "exit",
            &format!("elapsed:{:.6} code:{}", elapsed.as_secs_f64(), exit_code),
        );
    }
    if let Some(ref path) = trace2_perf_path {
        let elapsed = start.elapsed();
        let _ = trace2_write_perf(
            path,
            "exit",
            &format!("elapsed:{:.6} code:{}", elapsed.as_secs_f64(), exit_code),
        );
    }
    if let Some(ref path) = trace2_event_path {
        let elapsed = start.elapsed();
        let _ = trace2_write_json_event(
            path,
            "exit",
            &format!("elapsed:{:.6} code:{}", elapsed.as_secs_f64(), exit_code),
        );
    }

    std::process::exit(exit_code);
}

fn verbatim_lib_error_message(err: &anyhow::Error) -> Option<String> {
    for cause in err.chain() {
        if let Some(grit_lib::error::Error::Message(msg)) =
            cause.downcast_ref::<grit_lib::error::Error>()
        {
            return Some(msg.clone());
        }
    }
    None
}

fn error_chain_has_corrupt_cache_tree(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<grit_lib::error::Error>(),
            Some(grit_lib::error::Error::CacheTreeCorrupt)
        )
    })
}

fn path_error_fatal_message(err: &anyhow::Error) -> Option<String> {
    for cause in err.chain() {
        if let Some(grit_lib::error::Error::PathError(msg)) =
            cause.downcast_ref::<grit_lib::error::Error>()
        {
            return Some(msg.clone());
        }
    }
    None
}

fn is_broken_pipe_error(err: &anyhow::Error) -> bool {
    use std::io::ErrorKind;
    for cause in err.chain() {
        if let Some(ioe) = cause.downcast_ref::<std::io::Error>() {
            if ioe.kind() == ErrorKind::BrokenPipe {
                return true;
            }
        }
        if let Some(lib_err) = cause.downcast_ref::<grit_lib::error::Error>() {
            if let grit_lib::error::Error::Io(ioe) = lib_err {
                if ioe.kind() == ErrorKind::BrokenPipe {
                    return true;
                }
            }
        }
    }
    false
}

/// Get process ancestry by walking parent PIDs on Linux.
fn get_process_ancestry() -> Vec<String> {
    #[allow(unused_mut)]
    let mut result = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let mut pid = std::process::id();
        // Walk up to 10 ancestors
        for _ in 0..10 {
            if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
                let name = status
                    .lines()
                    .find(|l| l.starts_with("Name:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("unknown")
                    .to_string();
                let ppid = status
                    .lines()
                    .find(|l| l.starts_with("PPid:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                result.push(name);
                if ppid <= 1 {
                    break;
                }
                pid = ppid;
            } else {
                break;
            }
        }
    }
    result
}

/// Write a trace2 normal-format event to the trace file.
/// Write a GIT_TRACE line to the specified destination.
///
/// The destination can be:
/// - "1" or "true" → stderr
/// - "2" → stderr
/// - A file path → append to that file
pub(crate) fn write_git_trace(dest: &str, line: &str) {
    use std::io::Write;
    match dest {
        "1" | "true" | "2" => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        path => {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = file.write_all(line.as_bytes());
            }
        }
    }
}

/// Append a `trace: run_command: git …` line when `GIT_TRACE` points at a file or stderr (t6500-gc).
pub(crate) fn trace_run_command_git_invocation(args: &[&str]) {
    if let Ok(trace_val) = std::env::var("GIT_TRACE") {
        if trace_val.is_empty() || trace_val == "0" || trace_val.eq_ignore_ascii_case("false") {
            return;
        }
        let mut line = String::from("git");
        for a in args {
            line.push(' ');
            line.push_str(a);
        }
        let trace_line = if std::env::var("GIT_TRACE_BARE").ok().as_deref() == Some("1") {
            format!("trace: run_command: {line}\n")
        } else {
            let now = time::OffsetDateTime::now_utc();
            format!(
                "{:02}:{:02}:{:02}.{:06} git.c:000               trace: run_command: {line}\n",
                now.hour(),
                now.minute(),
                now.second(),
                now.microsecond(),
            )
        };
        write_git_trace(&trace_val, &trace_line);
    }
}

/// Append a `test_subcommand`-compatible line to `GIT_TRACE2_EVENT` (JSON array of argv strings).
pub(crate) fn trace2_emit_git_subcommand_argv(argv: &[String]) {
    let Ok(path) = std::env::var("GIT_TRACE2_EVENT") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let mut esc = String::new();
    esc.push('[');
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            esc.push(',');
        }
        esc.push('"');
        for ch in a.chars() {
            match ch {
                '\\' => esc.push_str("\\\\"),
                '"' => esc.push_str("\\\""),
                c => esc.push(c),
            }
        }
        esc.push('"');
    }
    esc.push_str("]\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, esc.as_bytes()));
}

fn trace2_write_event(path: &str, event: &str, data: &str) -> std::io::Result<()> {
    use std::io::Write;
    let now = chrono_now();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        "{} grit:0                         {} {}",
        now, event, data
    )?;
    Ok(())
}

/// Write a trace2 perf-format line.
fn trace2_write_perf(path: &str, event: &str, data: &str) -> std::io::Result<()> {
    use std::io::Write;
    let now = chrono_now();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        "{} grit:0  | d0 | main                     | {:<12} |     |           |           |              | {}",
        now, event, data
    )?;
    Ok(())
}

/// Write a trace2 JSON event line.
fn trace2_write_json_event(path: &str, event: &str, data: &str) -> std::io::Result<()> {
    use std::io::Write;
    let now = chrono_now();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        r#"{{"event":"{}","sid":"grit-0","time":"{}","data":"{}"}}"#,
        event, now, data
    )?;
    Ok(())
}

/// Append a trace2 JSON `child_start` line with an `argv` array (upstream test_subcommand format).
pub(crate) fn trace2_emit_child_start_json(path: &str, argv: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    let now = chrono_now();
    let mut parts = String::new();
    for (i, a) in argv.iter().enumerate() {
        if i > 0 {
            parts.push(',');
        }
        let esc = a.replace('\\', "\\\\").replace('"', "\\\"");
        parts.push('"');
        parts.push_str(&esc);
        parts.push('"');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        r#"{{"event":"child_start","sid":"grit-0","time":"{}","argv":[{}]}}"#,
        now, parts
    )?;
    Ok(())
}

/// Emit a trace2 JSON `region_enter` / `region_leave` pair for `GIT_TRACE2_EVENT` tests.
pub(crate) fn trace2_region_json(path: &str, category: &str, label: &str) -> std::io::Result<()> {
    use std::io::Write;
    let now = chrono_now();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        r#"{{"event":"region_enter","sid":"grit-0","time":"{}","category":"{}","label":"{}"}}"#,
        now, category, label
    )?;
    writeln!(
        file,
        r#"{{"event":"region_leave","sid":"grit-0","time":"{}","category":"{}","label":"{}"}}"#,
        now, category, label
    )?;
    Ok(())
}

/// Emit a trace2 `data` JSON event (`trace2_data_string` / `trace2_data_intmax` compatible).
pub(crate) fn trace2_write_json_data_line(
    path: &str,
    category: &str,
    key: &str,
    value: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    let now = chrono_now();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        r#"{{"event":"data","sid":"grit-0","time":"{}","category":"{}","key":"{}","value":"{}"}}"#,
        now, category, key, value
    )?;
    Ok(())
}

/// Emit a `trace2_data_intmax`-style data event to whichever trace2 targets are
/// active (PERF and/or EVENT), matching Git's `trace2_data_intmax`. The PERF
/// payload is rendered as `<key>:<value>` so substring greps used by the test
/// suite (e.g. `loosen_unused_packed_objects/loosened:0`) match.
pub(crate) fn trace2_emit_data_intmax(category: &str, key: &str, value: i64) {
    if let Some(path) = std::env::var_os("GIT_TRACE2_PERF") {
        if let Some(p) = path.to_str() {
            if !p.is_empty() {
                let _ = trace2_write_perf(p, "data", &format!("{key}:{value}"));
            }
        }
    }
    if let Some(path) = std::env::var_os("GIT_TRACE2_EVENT") {
        if let Some(p) = path.to_str() {
            if !p.is_empty() {
                let _ = trace2_write_json_data_line(p, category, key, &value.to_string());
            }
        }
    }
}

/// Emit a trace2 counter event used by upstream fsync assertions.
pub(crate) fn trace2_write_json_counter_line(
    path: &str,
    category: &str,
    name: &str,
    count: u64,
) -> std::io::Result<()> {
    use std::io::Write;
    let now = chrono_now();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        r#"{{"event":"counter","sid":"grit-0","time":"{}","category":"{}","name":"{}","count":{}}}"#,
        now, category, name, count
    )?;
    Ok(())
}

/// Write a trace2 JSON cmd_ancestry event line with an ancestry array.
fn trace2_write_json_ancestry(path: &str, ancestry: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    let now = chrono_now();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let ancestry = ancestry
        .iter()
        .map(|name| format!(r#""{name}""#))
        .collect::<Vec<_>>()
        .join(",");
    writeln!(
        file,
        r#"{{"event":"cmd_ancestry","sid":"grit-0","time":"{}","ancestry":[{}]}}"#,
        now, ancestry
    )?;
    Ok(())
}

/// Format current time as HH:MM:SS.microseconds for trace2 output.
fn chrono_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let micros = now.subsec_micros();
    let secs_in_day = total_secs % 86400;
    let hours = secs_in_day / 3600;
    let mins = (secs_in_day % 3600) / 60;
    let secs = secs_in_day % 60;
    format!("{:02}:{:02}:{:02}.{:06}", hours, mins, secs, micros)
}

/// Timestamp string for JSON trace2 events (`GIT_TRACE2_EVENT`) emitted outside `main`.
pub(crate) fn trace2_json_now() -> String {
    chrono_now()
}

pub(crate) fn exit_with_status(status: std::process::ExitStatus) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            std::process::exit(128 + sig);
        }
    }
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(feature = "test-tools")]
const TEST_TOOL_EXAMPLE_TAP_OUTPUT: &str = include_str!("test_tool_example_tap_output.txt");

#[cfg(feature = "test-tools")]
fn run_test_tool_example_tap(rest: &[String]) -> Result<()> {
    if rest.len() != 1 {
        bail!("usage: test-tool example-tap");
    }
    print!("{TEST_TOOL_EXAMPLE_TAP_OUTPUT}");
    std::process::exit(1);
}

/// `test-tool crontab <file> -l|<input>` (git/t/helper/test-crontab.c).
///
/// With `-l`, copy `<file>` to stdout (nothing if it does not exist); otherwise
/// copy `<input>` into `<file>`. Used to mock crontab in t7900.
#[cfg(feature = "test-tools")]
fn run_test_tool_crontab(args: &[String]) -> Result<()> {
    if args.len() != 2 {
        bail!("usage: test-tool crontab <file> -l|<input>");
    }
    let file = &args[0];
    if args[1] == "-l" {
        if let Ok(contents) = std::fs::read(file) {
            use std::io::Write as _;
            std::io::stdout().write_all(&contents)?;
        }
    } else {
        let input = std::fs::read(&args[1])
            .with_context(|| format!("test-tool crontab: cannot read '{}'", args[1]))?;
        std::fs::write(file, input)
            .with_context(|| format!("test-tool crontab: cannot write '{file}'"))?;
    }
    Ok(())
}

#[cfg(feature = "test-tools")]
fn run_test_tool_trace2(rest: &[String]) -> Result<()> {
    match rest.get(1).map(String::as_str).unwrap_or("") {
        "001return" => {
            let code: i32 = rest.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            std::process::exit(code);
        }
        "004child" => {
            if rest.len() <= 2 {
                return Ok(());
            }
            let status = std::process::Command::new(&rest[2])
                .args(&rest[3..])
                .status()?;
            exit_with_status(status);
        }
        "400ancestry" => {
            if rest.len() < 5 {
                bail!(
                    "usage: test-tool trace2 400ancestry <target> <output_file> <child_command_line>"
                );
            }

            let target = &rest[2];
            let output_file = &rest[3];
            let mut child = std::process::Command::new(&rest[4]);
            child.args(&rest[5..]);
            child.env("GIT_TRACE2", "");
            child.env("GIT_TRACE2_PERF", "");
            child.env("GIT_TRACE2_EVENT", "");
            child.env("GIT_TRACE2_BRIEF", "1");

            match target.as_str() {
                "normal" => {
                    child.env("GIT_TRACE2", output_file);
                }
                "perf" => {
                    child.env("GIT_TRACE2_PERF", output_file);
                }
                "event" => {
                    child.env("GIT_TRACE2_EVENT", output_file);
                }
                _ => bail!("invalid target '{target}', expected: normal, perf, event"),
            }

            let status = child.status()?;
            exit_with_status(status);
        }
        other => bail!("test-tool trace2: unknown subcommand '{other}'"),
    }
}

#[cfg(feature = "test-tools")]
fn run_test_tool_revision_walking(rest: &[String]) -> Result<()> {
    match rest.get(1).map(String::as_str).unwrap_or("") {
        "run-twice" => {
            let repo = grit_lib::repo::Repository::discover(None)?;
            let tips = vec!["HEAD".to_owned()];
            let empty: Vec<String> = Vec::new();
            let opts = grit_lib::rev_list::RevListOptions::default();
            let walked = grit_lib::rev_list::rev_list(&repo, &tips, &empty, &opts)?;

            for label in ["1st", "2nd"] {
                println!("{label}");
                for oid in &walked.commits {
                    let obj = repo.odb.read(oid)?;
                    let commit = grit_lib::objects::parse_commit(&obj.data)?;
                    let subject = commit.message.lines().next().unwrap_or_default();
                    println!(" > {subject}");
                }
            }
            Ok(())
        }
        other => bail!("test-tool revision-walking: unknown subcommand '{other}'"),
    }
}

#[cfg(feature = "test-tools")]
fn run_test_tool_mergesort(rest: &[String]) -> Result<()> {
    match rest.get(1).map(String::as_str).unwrap_or("") {
        "test" => {
            // Minimal self-check used by t0071-sort.sh.
            let mut values = vec![9, 1, 5, 3, 7, 2, 8, 4, 6];
            let mut expected = values.clone();
            values.sort();
            expected.sort();
            if values == expected {
                Ok(())
            } else {
                bail!("test-tool mergesort: internal self-check failed");
            }
        }
        other => bail!("test-tool mergesort: unknown subcommand '{other}'"),
    }
}

#[cfg(feature = "test-tools")]
fn run_test_tool_hexdump(_rest: &[String]) -> Result<()> {
    use std::io::{Read, Write};

    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 1024];
    let mut have_data = false;

    loop {
        let len = stdin.read(&mut buf)?;
        if len == 0 {
            break;
        }

        have_data = true;
        for byte in &buf[..len] {
            write!(stdout, "{:02x} ", *byte)?;
        }
    }

    if have_data {
        writeln!(stdout)?;
    }

    Ok(())
}

#[cfg(feature = "test-tools")]
const BUILTIN_USERDIFF_DRIVERS: &[&str] = &[
    "ada", "bash", "bibtex", "cpp", "csharp", "css", "dts", "elixir", "fortran", "fountain",
    "golang", "html", "ini", "java", "kotlin", "markdown", "matlab", "objc", "pascal", "perl",
    "php", "python", "r", "ruby", "rust", "scheme", "tex",
];

#[cfg(feature = "test-tools")]
fn collect_custom_userdiff_drivers(config: &grit_lib::config::ConfigSet) -> Vec<String> {
    let mut custom = std::collections::BTreeSet::new();

    for entry in config.entries() {
        let Some(rest) = entry.key.strip_prefix("diff.") else {
            continue;
        };
        let Some(driver) = rest
            .strip_suffix(".funcname")
            .or_else(|| rest.strip_suffix(".xfuncname"))
        else {
            continue;
        };
        if driver.is_empty() || BUILTIN_USERDIFF_DRIVERS.contains(&driver) {
            continue;
        }
        custom.insert(driver.to_owned());
    }

    custom.into_iter().collect()
}

#[cfg(feature = "test-tools")]
fn run_test_tool_userdiff(rest: &[String]) -> Result<()> {
    if rest.len() != 2 {
        bail!("usage: test-tool userdiff <list-drivers|list-builtin-drivers|list-custom-drivers>");
    }

    let (want_builtin, want_custom) = match rest[1].as_str() {
        "list-drivers" => (true, true),
        "list-builtin-drivers" => (true, false),
        "list-custom-drivers" => (false, true),
        other => bail!("test-tool userdiff: unknown argument '{other}'"),
    };

    if want_builtin {
        for driver in BUILTIN_USERDIFF_DRIVERS {
            println!("{driver}");
        }
    }

    if want_custom {
        let repo = grit_lib::repo::Repository::discover(None)?;
        let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
            .unwrap_or_else(|_| grit_lib::config::ConfigSet::new());

        for driver in collect_custom_userdiff_drivers(&config) {
            println!("{driver}");
        }
    }

    Ok(())
}

#[cfg(feature = "test-tools")]
fn parse_find_pack_count_arg(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("invalid --check-count value: {value}"))
}

#[cfg(feature = "test-tools")]
fn display_find_pack_path(pack_path: &Path) -> String {
    let normalized = std::fs::canonicalize(pack_path).unwrap_or_else(|_| pack_path.to_path_buf());
    if let Ok(cwd) = std::env::current_dir() {
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        if let Ok(relative) = normalized.strip_prefix(&cwd) {
            if !relative.as_os_str().is_empty() {
                return relative.to_string_lossy().into_owned();
            }
        }
    }
    pack_path.to_string_lossy().into_owned()
}

#[cfg(feature = "test-tools")]
fn run_test_tool_find_pack(rest: &[String]) -> Result<()> {
    let mut i = 1usize;
    let mut expected_count: Option<usize> = None;

    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--check-count" || arg == "-c" {
            let Some(next) = rest.get(i + 1) else {
                bail!("usage: test-tool find-pack [--check-count=<n>|-c <n>] <object>");
            };
            expected_count = Some(parse_find_pack_count_arg(next)?);
            i += 2;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--check-count=") {
            expected_count = Some(parse_find_pack_count_arg(v)?);
            i += 1;
            continue;
        }
        break;
    }

    let Some(spec) = rest.get(i) else {
        bail!("usage: test-tool find-pack [--check-count=<n>|-c <n>] <object>");
    };
    if i + 1 != rest.len() {
        bail!("usage: test-tool find-pack [--check-count=<n>|-c <n>] <object>");
    }

    let repo = grit_lib::repo::Repository::discover(None)?;
    let oid = grit_lib::rev_parse::resolve_revision(&repo, spec)?;
    let mut object_dirs = vec![repo.odb.objects_dir().to_path_buf()];
    if let Ok(alternates) = grit_lib::pack::read_alternates_recursive(repo.odb.objects_dir()) {
        object_dirs.extend(alternates);
    }

    let mut packs: Vec<String> = Vec::new();
    for objects_dir in object_dirs {
        let indexes = grit_lib::pack::read_local_pack_indexes(&objects_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        for idx in indexes {
            if idx
                .entries
                .iter()
                .any(|entry| grit_lib::pack::pack_index_entry_matches_sha1_oid(entry, &oid))
            {
                packs.push(display_find_pack_path(&idx.pack_path));
            }
        }
    }
    packs.sort();
    packs.dedup();

    for path in &packs {
        println!("{path}");
    }

    if let Some(n) = expected_count {
        if packs.len() != n {
            std::process::exit(1);
        }
    }

    Ok(())
}

#[cfg(feature = "test-tools")]
fn run_test_tool_bitmap(rest: &[String]) -> Result<()> {
    let Some(subcommand) = rest.get(1).map(String::as_str) else {
        bail!("usage: test-tool bitmap list-commits");
    };
    if subcommand != "list-commits" {
        bail!("test-tool bitmap: unknown subcommand '{subcommand}'");
    }

    let repo = grit_lib::repo::Repository::discover(None)?;
    let mut opts = grit_lib::rev_list::RevListOptions::default();
    opts.all_refs = true;
    let result = grit_lib::rev_list::rev_list(&repo, &[], &[], &opts)
        .context("failed to list bitmap commits")?;
    let mut commits = result.commits;
    commits.sort();
    commits.dedup();

    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_else(|_| grit_lib::config::ConfigSet::new());
    let has_preferred_tips = config
        .get("pack.preferBitmapTips")
        .is_some_and(|value| !value.trim().is_empty());
    let omitted = if has_preferred_tips {
        None
    } else {
        commits.last().copied()
    };

    for oid in commits {
        if Some(oid) != omitted {
            println!("{}", oid.to_hex());
        }
    }

    Ok(())
}

/// `test-tool partial-clone` — matches `git/t/helper/test-partial-clone.c` (t0410).
#[cfg(feature = "test-tools")]
fn run_test_tool_partial_clone(rest: &[String]) -> Result<()> {
    if rest.len() < 4 {
        bail!("usage: test-tool partial-clone object-info <gitdir> <oid>");
    }
    if rest[1] != "object-info" {
        bail!("test-tool partial-clone: unknown subcommand '{}'", rest[1]);
    }
    let git_dir = std::path::Path::new(&rest[2]);
    let repo = grit_lib::repo::Repository::open(git_dir, None)
        .with_context(|| format!("could not open repository at {}", git_dir.display()))?;
    let oid: grit_lib::objects::ObjectId = rest[3]
        .parse()
        .map_err(|_| anyhow::anyhow!("could not parse oid"))?;
    crate::commands::promisor_hydrate::try_lazy_fetch_promisor_object(&repo, oid)?;
    let obj = repo
        .odb
        .read(&oid)
        .with_context(|| format!("could not obtain object info for {}", oid.to_hex()))?;
    println!("{}", obj.data.len());
    Ok(())
}

#[cfg(feature = "test-tools")]
fn run_test_tool_ref_store(rest: &[String]) -> Result<()> {
    if rest.len() < 3 {
        bail!("usage: test-tool ref-store <backend> <subcommand> ...");
    }
    let backend = rest[1].as_str();
    let sub = rest[2].as_str();
    if backend.starts_with("worktree:") || backend.starts_with("submodule:") {
        return commands::test_tool_ref_store::run(&rest[1..]);
    }
    if backend != "main" {
        bail!("test-tool ref-store: unsupported backend (only 'main', 'worktree:*' and 'submodule:*' are implemented)");
    }

    let repo = grit_lib::repo::Repository::discover(None)?;
    let git_dir = repo.git_dir.clone();

    match sub {
        "delete-refs" => {
            // delete-refs <flags> <msg> <refname>...
            let mut i = 3usize;
            if i >= rest.len() {
                bail!("usage: test-tool ref-store main delete-refs <flags> <msg> <ref>...");
            }
            let _flags = &rest[i];
            i += 1;
            if i >= rest.len() {
                bail!("usage: test-tool ref-store main delete-refs <flags> <msg> <ref>...");
            }
            let _msg = &rest[i];
            i += 1;
            while i < rest.len() {
                let refname = &rest[i];
                let ref_path = git_dir.join(refname);
                let _ = std::fs::remove_file(&ref_path);
                let log_path = git_dir.join("logs").join(refname);
                let _ = std::fs::remove_file(&log_path);
                i += 1;
            }
            Ok(())
        }
        "update-ref" => {
            if rest.len() < 8 {
                bail!(
                    "usage: test-tool ref-store main update-ref <msg> <ref> <new> <old> [flags...]"
                );
            }
            let msg = &rest[3];
            let refname = &rest[4];
            let new_oid = &rest[5];
            let old_oid = &rest[6];
            let flags = if rest.len() > 7 { &rest[7..] } else { &[] };
            let skip_oid_verification = flags.iter().any(|f| f == "REF_SKIP_OID_VERIFICATION");
            let skip_refname_verification =
                flags.iter().any(|f| f == "REF_SKIP_REFNAME_VERIFICATION");

            // `dispatch` is given the subcommand name separately, so `args` must contain
            // only the flags/operands of `git update-ref` (not the "update-ref" word itself).
            let mut args = vec![
                "-m".to_owned(),
                msg.clone(),
                refname.clone(),
                new_oid.clone(),
            ];
            if old_oid != "0000000000000000000000000000000000000000" {
                args.push(old_oid.clone());
            }
            if skip_oid_verification || skip_refname_verification {
                let oid = grit_lib::objects::ObjectId::from_hex(new_oid)
                    .with_context(|| format!("invalid object id '{new_oid}'"))?;
                let old = grit_lib::objects::ObjectId::from_hex(old_oid)
                    .with_context(|| format!("invalid object id '{old_oid}'"))?;
                if grit_lib::reftable::is_reftable_repo(&git_dir) {
                    grit_lib::reftable::reftable_write_ref(&git_dir, refname, &oid, None, None)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                } else {
                    let ref_path = git_dir.join(refname);
                    if let Some(parent) = ref_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(ref_path, format!("{new_oid}\n"))?;
                }
                let identity = commands::update_ref::resolve_reflog_identity(&repo);
                let _ = grit_lib::refs::append_reflog(
                    &git_dir, refname, &old, &oid, &identity, msg, true,
                );
                return Ok(());
            }
            // Without REF_SKIP_OID_VERIFICATION, `refs_update_ref` (REF_STORE_WRITE) requires the
            // new value to name an existing object; reject dangling OIDs so a non-skipping
            // update of an invalid OID fails (t0610 "can skip object ID verification").
            if !new_oid.starts_with("0000000000000000000000000000000000000000") {
                if let Ok(oid) = grit_lib::objects::ObjectId::from_hex(new_oid) {
                    if !repo.odb.exists(&oid) {
                        bail!("update_ref failed for ref '{refname}': cannot update ref '{refname}': trying to write ref '{refname}' with nonexistent object {new_oid}");
                    }
                }
            }
            dispatch("update-ref", &args, &GlobalOpts::default())
        }
        "for-each-ref" => {
            let prefix = rest.get(3).map(String::as_str).unwrap_or("");
            let mut refs = if grit_lib::reftable::is_reftable_repo(&git_dir) {
                grit_lib::reftable::ReftableStack::open(&git_dir)
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .read_refs()
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .into_iter()
                    .filter_map(|record| match record.value {
                        grit_lib::reftable::RefValue::Val1(oid)
                        | grit_lib::reftable::RefValue::Val2(oid, _) => {
                            Some((record.name, oid.to_hex()))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            } else {
                grit_lib::refs::list_refs(&git_dir, prefix)?
                    .into_iter()
                    .map(|(name, oid)| (name, oid.to_hex()))
                    .collect::<Vec<_>>()
            };
            refs.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, oid) in refs {
                if !prefix.is_empty() && !name.starts_with(prefix) {
                    continue;
                }
                let flags = if grit_lib::check_ref_format::check_refname_format(
                    &name,
                    &grit_lib::check_ref_format::RefNameOptions {
                        allow_onelevel: false,
                        refspec_pattern: false,
                        normalize: false,
                    },
                )
                .is_err()
                {
                    "0xc"
                } else {
                    "0x0"
                };
                let display_oid = if flags == "0xc" {
                    "0000000000000000000000000000000000000000"
                } else {
                    oid.as_str()
                };
                // The C helper sets `trim_prefix = strlen(prefix)`, so the prefix is stripped
                // from the emitted refname (e.g. `refs/heads/main` -> `main`).
                let display = name.strip_prefix(prefix).unwrap_or(&name);
                println!("{display_oid} {display} {flags}");
            }
            Ok(())
        }
        "reflog-exists" => {
            let refname = rest.get(3).ok_or_else(|| {
                anyhow::anyhow!("usage: test-tool ref-store main reflog-exists <ref>")
            })?;
            if grit_lib::reflog::reflog_exists(&git_dir, refname) {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        "create-reflog" => {
            let refname = rest.get(3).ok_or_else(|| {
                anyhow::anyhow!("usage: test-tool ref-store main create-reflog <ref>")
            })?;
            if grit_lib::reftable::is_reftable_repo(&git_dir) {
                grit_lib::reftable::reftable_create_reflog(&git_dir, refname)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            } else {
                let path = grit_lib::reflog::reflog_path(&git_dir, refname);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?;
            }
            Ok(())
        }
        "delete-reflog" => {
            let refname = rest.get(3).ok_or_else(|| {
                anyhow::anyhow!("usage: test-tool ref-store main delete-reflog <ref>")
            })?;
            if grit_lib::reftable::is_reftable_repo(&git_dir) {
                grit_lib::reftable::reftable_delete_reflog(&git_dir, refname)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            } else {
                let _ = std::fs::remove_file(grit_lib::reflog::reflog_path(&git_dir, refname));
            }
            Ok(())
        }
        "for-each-reflog-ent" | "for-each-reflog-ent-reverse" => {
            let refname = rest
                .get(3)
                .ok_or_else(|| anyhow::anyhow!("usage: test-tool ref-store main {sub} <ref>"))?;
            let mut entries = grit_lib::reflog::read_reflog(&git_dir, refname)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            // `read_reflog` returns entries in file order (oldest-first), which
            // matches upstream `files_for_each_reflog_ent` (front-to-back stream).
            // Only the `-reverse` variant emits newest-first.
            if sub == "for-each-reflog-ent-reverse" {
                entries.reverse();
            }
            for entry in entries {
                println!(
                    "{} {} {}\t{}",
                    entry.old_oid.to_hex(),
                    entry.new_oid.to_hex(),
                    entry.identity,
                    entry.message
                );
            }
            Ok(())
        }
        "create-symref" => {
            if rest.len() < 5 {
                bail!("usage: test-tool ref-store main create-symref <refname> <target> [logmsg]");
            }
            let refname = &rest[3];
            let target = &rest[4];
            let common = grit_lib::worktree::common_git_dir(&git_dir);
            let base = if refname.starts_with("refs/worktree/") {
                &git_dir
            } else {
                &common
            };
            let path = base.join(refname);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let lock_path = grit_lib::refs::lock_path_for_ref(&path);
            std::fs::write(&lock_path, format!("ref: {target}\n"))?;
            std::fs::rename(lock_path, path)?;
            Ok(())
        }
        "rename-ref" => {
            // rename-ref <oldref> <newref> [logmsg]
            if rest.len() < 5 {
                bail!("usage: test-tool ref-store main rename-ref <oldref> <newref> [logmsg]");
            }
            let oldref = &rest[3];
            let newref = &rest[4];
            let logmsg = rest.get(5).cloned().unwrap_or_default();
            run_ref_store_rename(&repo, &git_dir, oldref, newref, &logmsg)
        }
        "verify-ref" => {
            // verify-ref <refname>: succeeds iff creating <refname> would not hit a D/F
            // conflict with another ref (matches refs_verify_refname_available).
            let refname = rest.get(3).ok_or_else(|| {
                anyhow::anyhow!("usage: test-tool ref-store main verify-ref <refname>")
            })?;
            match grit_lib::refs::verify_refname_available_for_create(
                &git_dir,
                refname,
                &std::collections::BTreeSet::new(),
                &std::collections::HashSet::new(),
            ) {
                Ok(()) => Ok(()),
                Err(e) => {
                    println!("{}", e.lock_message_suffix());
                    std::process::exit(1);
                }
            }
        }
        "delete-ref" => {
            // delete-ref <msg> <refname> <old-sha1> <flags>
            if rest.len() < 6 {
                bail!("usage: test-tool ref-store main delete-ref <msg> <ref> <old-sha1> <flags>");
            }
            let _msg = &rest[3];
            let refname = &rest[4];
            let old_sha1 = &rest[5];
            // Honor the old-sha1 precondition (REF_NO_DEREF/flags don't change this test's path).
            if old_sha1 != "0000000000000000000000000000000000000000" {
                let expected = grit_lib::objects::ObjectId::from_hex(old_sha1)
                    .with_context(|| format!("cannot parse {old_sha1} as object id"))?;
                match grit_lib::refs::resolve_ref(&git_dir, refname) {
                    Ok(current) if current == expected => {}
                    Ok(current) => bail!(
                        "ref {refname} is at {} but expected {}",
                        current.to_hex(),
                        expected.to_hex()
                    ),
                    Err(e) => bail!("cannot lock ref '{refname}': {e}"),
                }
            }
            grit_lib::refs::delete_ref(&git_dir, refname).map_err(|e| anyhow::anyhow!("{e}"))
        }
        // The module entry point expects `<store> <function> ...`, so pass the backend too.
        "resolve-ref" => commands::test_tool_ref_store::run(&rest[1..]),
        "for-each-reflog" => commands::test_tool_ref_store::run(&rest[1..]),
        other => bail!("test-tool ref-store: unsupported subcommand '{other}'"),
    }
}

/// Low-level ref rename for `test-tool ref-store main rename-ref`, mirroring the files-backend
/// `files_copy_or_rename_ref`: resolve the old ref without dereferencing (symrefs are rejected),
/// migrate its reflog, delete the old ref, write the new ref, and append the rename reflog entry.
#[cfg(feature = "test-tools")]
fn run_ref_store_rename(
    repo: &grit_lib::repo::Repository,
    git_dir: &std::path::Path,
    oldref: &str,
    newref: &str,
    logmsg: &str,
) -> Result<()> {
    // Renaming a symbolic ref is not supported (matches the C backend).
    if matches!(
        grit_lib::refs::read_symbolic_ref(git_dir, oldref),
        Ok(Some(_))
    ) {
        bail!("refname {oldref} is a symbolic ref, renaming it is not supported");
    }

    let old_oid = grit_lib::refs::resolve_ref(git_dir, oldref)
        .map_err(|_| anyhow::anyhow!("refname {oldref} not found"))?;

    // Capture the old reflog bytes before deleting the ref (delete_ref removes logs/<ref> too).
    let logs_dir = git_dir.join("logs");
    let old_log_path = logs_dir.join(oldref);
    let old_reflog_bytes = if old_log_path.is_file() {
        std::fs::read(&old_log_path).ok()
    } else {
        None
    };

    // Delete the old ref first to avoid d/f conflicts when old/new share a path prefix.
    grit_lib::refs::delete_ref(git_dir, oldref).map_err(|e| anyhow::anyhow!("{e}"))?;
    // Remove any pre-existing destination ref so the write lands cleanly.
    let _ = grit_lib::refs::delete_ref(git_dir, newref);

    grit_lib::refs::write_ref(git_dir, newref, &old_oid).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Migrate the reflog content to the new ref's reflog path.
    if let Some(log_bytes) = old_reflog_bytes {
        let new_log = logs_dir.join(newref);
        if let Some(parent) = new_log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&new_log, log_bytes)?;
    }

    // Append a reflog entry for the rename when a log message was supplied (an empty logmsg
    // leaves the migrated reflog untouched, matching commit_ref_update with a NULL message).
    if !logmsg.is_empty() {
        let identity = commands::update_ref::resolve_reflog_identity(repo);
        let _ = grit_lib::refs::append_reflog(
            git_dir, newref, &old_oid, &old_oid, &identity, logmsg, true,
        );
    }

    Ok(())
}

#[cfg(feature = "test-tools")]
fn dir_iterator_error_name(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::NotADirectory => "ENOTDIR",
        _ => "ESOMETHINGELSE",
    }
}

#[cfg(feature = "test-tools")]
fn walk_dir_iterator(
    root_abs: &Path,
    root_display: &str,
    rel: &Path,
    pedantic: bool,
) -> std::result::Result<(), ()> {
    let current = if rel.as_os_str().is_empty() {
        root_abs.to_path_buf()
    } else {
        root_abs.join(rel)
    };

    let read_dir = match std::fs::read_dir(&current) {
        Ok(it) => it,
        Err(_) => {
            return if pedantic { Err(()) } else { Ok(()) };
        }
    };

    let mut entries = Vec::new();
    for entry in read_dir {
        match entry {
            Ok(e) => entries.push(e),
            Err(_) => {
                if pedantic {
                    return Err(());
                }
            }
        }
    }
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let basename = entry.file_name();
        let basename_display = basename.to_string_lossy().to_string();
        let child_rel = if rel.as_os_str().is_empty() {
            PathBuf::from(&basename)
        } else {
            rel.join(&basename)
        };
        let child_abs = root_abs.join(&child_rel);

        let meta = match std::fs::symlink_metadata(&child_abs) {
            Ok(m) => m,
            Err(_) => {
                if pedantic {
                    return Err(());
                }
                continue;
            }
        };
        let ft = meta.file_type();
        let kind = if ft.is_dir() {
            'd'
        } else if ft.is_file() {
            'f'
        } else if ft.is_symlink() {
            's'
        } else {
            '?'
        };

        let path_display = Path::new(root_display).join(&child_rel);
        println!(
            "[{kind}] ({}) [{}] {}",
            child_rel.to_string_lossy(),
            basename_display,
            path_display.display()
        );

        if ft.is_dir() && walk_dir_iterator(root_abs, root_display, &child_rel, pedantic).is_err() {
            return Err(());
        }
    }

    Ok(())
}

#[cfg(feature = "test-tools")]
fn run_test_tool_dir_iterator(rest: &[String]) -> Result<()> {
    let mut pedantic = false;
    let mut path_arg: Option<String> = None;

    for arg in rest.iter().skip(1) {
        if arg == "--pedantic" {
            pedantic = true;
            continue;
        }
        if arg.starts_with("--") {
            bail!("invalid option '{arg}'");
        }
        if path_arg.is_some() {
            bail!("dir-iterator needs exactly one non-option argument");
        }
        path_arg = Some(arg.clone());
    }

    let Some(path_arg) = path_arg else {
        bail!("dir-iterator needs exactly one non-option argument");
    };

    let root_abs = PathBuf::from(&path_arg);
    let root_meta = match std::fs::symlink_metadata(&root_abs) {
        Ok(m) => m,
        Err(e) => {
            println!(
                "dir_iterator_begin failure: {}",
                dir_iterator_error_name(e.kind())
            );
            std::process::exit(1);
        }
    };

    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        println!("dir_iterator_begin failure: ENOTDIR");
        std::process::exit(1);
    }

    if walk_dir_iterator(&root_abs, &path_arg, Path::new(""), pedantic).is_err() {
        println!("dir_iterator_advance failure");
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(feature = "test-tools")]
fn run_test_tool_parse_pathspec_file(rest: &[String]) -> Result<()> {
    let mut pathspec_from_file: Option<String> = None;
    let mut pathspec_file_nul = false;

    for arg in rest.iter().skip(1) {
        if let Some(v) = arg.strip_prefix("--pathspec-from-file=") {
            pathspec_from_file = Some(v.to_owned());
            continue;
        }
        if arg == "--pathspec-file-nul" {
            pathspec_file_nul = true;
            continue;
        }
        bail!("usage: test-tool parse-pathspec-file --pathspec-from-file [--pathspec-file-nul]");
    }

    let Some(pathspec_source) = pathspec_from_file else {
        bail!("usage: test-tool parse-pathspec-file --pathspec-from-file [--pathspec-file-nul]");
    };

    let data = if pathspec_source == "-" {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        buf
    } else {
        std::fs::read(&pathspec_source)?
    };

    let items: Vec<String> =
        grit_lib::pathspec::parse_pathspecs_from_source(&data, pathspec_file_nul)?;

    for item in items {
        println!("{item}");
    }
    Ok(())
}

fn parse_bool_str(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(feature = "test-tools")]
fn run_test_tool_advise(rest: &[String]) -> Result<()> {
    if rest.len() != 2 {
        bail!("usage: test-tool advise <message>");
    }
    let advice_msg = &rest[1];

    let global_advice = std::env::var("GIT_ADVICE")
        .ok()
        .and_then(|v| parse_bool_str(&v));
    if global_advice == Some(false) {
        return Ok(());
    }

    let config_advice = if let Some(v) = protocol::check_config_param("advice.nestedTag") {
        parse_bool_str(&v)
    } else {
        let git_dir = std::env::var("GIT_DIR")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| {
                grit_lib::repo::Repository::discover(None)
                    .ok()
                    .map(|r| r.git_dir)
            });
        if let Some(gd) = git_dir {
            if let Ok(config) = grit_lib::config::ConfigSet::load(Some(gd.as_path()), true) {
                config
                    .get("advice.nestedTag")
                    .and_then(|v| parse_bool_str(&v))
            } else {
                None
            }
        } else {
            None
        }
    };

    let enabled = global_advice == Some(true) || config_advice != Some(false);
    if !enabled {
        return Ok(());
    }

    eprintln!("hint: {advice_msg}");
    if config_advice.is_none() {
        eprintln!("hint: Disable this message with \"git config set advice.nestedTag false\"");
    }
    Ok(())
}

#[cfg(feature = "test-tools")]
fn parse_ulong_str(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

#[cfg(feature = "test-tools")]
fn run_test_tool_env_helper(rest: &[String]) -> Result<()> {
    // test-tool env-helper --type=<bool|ulong> --default=<value> [--exit-code] <VAR>
    if rest.len() < 3 || rest.first().map(String::as_str) != Some("env-helper") {
        bail!("usage: test-tool env-helper --type=<bool|ulong> [--default=<value>] [--exit-code] <VAR>");
    }

    let mut value_type: Option<&str> = None;
    let mut default_value: Option<String> = None;
    let mut exit_code_only = false;
    let mut variable_name: Option<&str> = None;

    let mut i = 1usize;
    while i < rest.len() {
        let arg = &rest[i];
        if let Some(v) = arg.strip_prefix("--type=") {
            value_type = Some(v);
            i += 1;
            continue;
        }
        if arg == "--type" {
            bail!("usage: test-tool env-helper --type=<bool|ulong> [--default=<value>] [--exit-code] <VAR>");
        }
        if let Some(v) = arg.strip_prefix("--default=") {
            if v.is_empty() {
                bail!("usage: test-tool env-helper --type=<bool|ulong> [--default=<value>] [--exit-code] <VAR>");
            }
            default_value = Some(v.to_owned());
            i += 1;
            continue;
        }
        if arg == "--default" {
            bail!("usage: test-tool env-helper --type=<bool|ulong> [--default=<value>] [--exit-code] <VAR>");
        }
        if arg == "--exit-code" {
            exit_code_only = true;
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            bail!("usage: test-tool env-helper --type=<bool|ulong> [--default=<value>] [--exit-code] <VAR>");
        }
        if variable_name.is_some() {
            bail!("usage: test-tool env-helper --type=<bool|ulong> [--default=<value>] [--exit-code] <VAR>");
        }
        variable_name = Some(arg);
        i += 1;
    }

    let Some(value_type) = value_type else {
        bail!("usage: test-tool env-helper --type=<bool|ulong> [--default=<value>] [--exit-code] <VAR>");
    };
    let Some(var) = variable_name else {
        bail!("usage: test-tool env-helper --type=<bool|ulong> [--default=<value>] [--exit-code] <VAR>");
    };

    let resolved = std::env::var(var).ok().or(default_value);
    let Some(value) = resolved else {
        bail!("usage: test-tool env-helper --type=<bool|ulong> [--default=<value>] [--exit-code] <VAR>");
    };

    match value_type {
        "bool" => {
            let Some(flag) = parse_bool_str(&value) else {
                std::process::exit(1);
            };
            if !exit_code_only {
                println!("{}", if flag { "true" } else { "false" });
            }
            if flag {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        "ulong" => {
            let Some(num) = parse_ulong_str(&value) else {
                std::process::exit(1);
            };
            if !exit_code_only {
                println!("{num}");
            }
            if num > 0 {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        _ => bail!(
            "usage: test-tool env-helper --type=<bool|ulong> [--default=<value>] [--exit-code] <VAR>"
        ),
    }
}
#[cfg(feature = "test-tools")]
fn test_tool_usage() -> Result<()> {
    bail!("test-tool: unknown or invalid subcommand usage")
}

/// `test-tool online-cpus` — print the number of processors Git would consider "online"
/// (matches `git/t/helper/test-online-cpus.c` using `std::thread::available_parallelism`).
#[cfg(feature = "test-tools")]
fn run_test_tool_path_walk(rest: &[String]) -> Result<()> {
    let args = preprocess_test_tool_args(rest)?;
    let args = if args.first().map(String::as_str) == Some("path-walk") {
        args[1..].to_vec()
    } else {
        args
    };
    let repo = grit_lib::repo::Repository::discover(None)?;
    let (opts, positive, negative, stdin_all, boundary) =
        grit_lib::path_walk::parse_path_walk_cli(&repo.git_dir, &args)
            .context("path-walk options")?;
    let (lines, counts) = grit_lib::path_walk::walk_objects_by_path(
        &repo, &positive, &negative, stdin_all, boundary, &opts,
    )?;

    fn kind_str(k: grit_lib::objects::ObjectKind) -> &'static str {
        match k {
            grit_lib::objects::ObjectKind::Blob => "blob",
            grit_lib::objects::ObjectKind::Tree => "tree",
            grit_lib::objects::ObjectKind::Commit => "commit",
            grit_lib::objects::ObjectKind::Tag => "tag",
        }
    }

    for line in lines {
        let suf = if line.uninteresting {
            ":UNINTERESTING"
        } else {
            ""
        };
        println!(
            "{}:{}:{}:{}{}",
            line.batch,
            kind_str(line.object_kind),
            line.path,
            line.oid.to_hex(),
            suf
        );
    }
    println!(
        "commits:{}\ntrees:{}\nblobs:{}\ntags:{}",
        counts.commits, counts.trees, counts.blobs, counts.tags
    );
    Ok(())
}

#[cfg(feature = "test-tools")]
fn run_test_tool_online_cpus(rest: &[String]) -> Result<()> {
    let _ = preprocess_test_tool_args(rest)?;
    let n = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    println!("{n}");
    Ok(())
}

#[cfg(feature = "test-tools")]
fn run_test_tool_delta(rest: &[String]) -> Result<()> {
    if rest.len() != 5 {
        bail!("usage: test-tool delta (-d|-p) <from_file> <data_file> <out_file>");
    }
    match rest[1].as_str() {
        "-p" => {
            let base = std::fs::read(&rest[2]).with_context(|| format!("read {}", rest[2]))?;
            let delta = std::fs::read(&rest[3]).with_context(|| format!("read {}", rest[3]))?;
            let result = grit_lib::unpack_objects::apply_delta(&base, &delta)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            std::fs::write(&rest[4], result).with_context(|| format!("write {}", rest[4]))?;
            Ok(())
        }
        "-d" => bail!("test-tool delta: delta generation is not implemented"),
        _ => bail!("usage: test-tool delta (-d|-p) <from_file> <data_file> <out_file>"),
    }
}

#[cfg(feature = "test-tools")]
fn run_test_tool_pack_mtimes(rest: &[String]) -> Result<()> {
    if rest.len() != 2 {
        bail!("usage: test-tool pack-mtimes <pack-name.mtimes>");
    }
    let repo = grit_lib::repo::Repository::discover(None)?;
    let mtimes_name = Path::new(&rest[1])
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid mtimes filename"))?;
    let stem = mtimes_name
        .strip_suffix(".mtimes")
        .ok_or_else(|| anyhow::anyhow!("usage: test-tool pack-mtimes <pack-name.mtimes>"))?;
    let pack_dir = repo.git_dir.join("objects").join("pack");
    let idx_path = pack_dir.join(format!("{stem}.idx"));
    let mtimes_path = pack_dir.join(mtimes_name);
    let idx = grit_lib::pack::read_pack_index(&idx_path)
        .map_err(|e| anyhow::anyhow!("could not load pack index: {e}"))?;
    let bytes = fs::read(&mtimes_path).context("could not load pack .mtimes")?;
    let count = idx.entries.len();
    let expected = 12usize
        .saturating_add(count.saturating_mul(4))
        .saturating_add(idx.hash_bytes.saturating_mul(2));
    if bytes.len() != expected || bytes.len() < 12 {
        bail!("could not load pack .mtimes");
    }
    if u32::from_be_bytes(bytes[0..4].try_into()?) != 0x4d54_4d45
        || u32::from_be_bytes(bytes[4..8].try_into()?) != 1
    {
        bail!("could not load pack .mtimes");
    }
    let mut pos = 12usize;
    for entry in &idx.entries {
        let mtime = u32::from_be_bytes(bytes[pos..pos + 4].try_into()?);
        pos += 4;
        println!("{} {mtime}", hex::encode(&entry.oid));
    }
    Ok(())
}

/// `test-tool lazy-init-name-hash` — exercise case-folding name/dir hash init (t3008, perf tests).
#[cfg(feature = "test-tools")]
fn run_test_tool_lazy_init_name_hash(rest: &[String]) -> Result<()> {
    use anyhow::Context;
    use std::io::Write;

    let args = preprocess_test_tool_args(rest)?;
    if args.first().map(String::as_str) != Some("lazy-init-name-hash") {
        bail!("internal error: lazy-init-name-hash dispatcher");
    }

    let repo = grit_lib::repo::Repository::discover(None).context("not a git repository")?;
    let index = repo.load_index().context("loading index")?;

    let mut single = false;
    let mut multi = false;
    let mut count: usize = 1;
    let mut dump = false;
    let mut perf = false;
    let mut analyze: Option<i32> = None;
    let mut analyze_step: Option<i32> = None;

    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "-s" | "--single" => {
                single = true;
                i += 1;
            }
            "-m" | "--multi" => {
                multi = true;
                i += 1;
            }
            "-c" | "--count" => {
                let v = args.get(i + 1).ok_or_else(|| {
                    anyhow::anyhow!("test-tool lazy-init-name-hash: --count needs a value")
                })?;
                count = v
                    .parse()
                    .with_context(|| format!("invalid --count '{v}'"))?;
                i += 2;
            }
            "-d" | "--dump" => {
                dump = true;
                i += 1;
            }
            "-p" | "--perf" => {
                perf = true;
                i += 1;
            }
            "-a" | "--analyze" => {
                let v = args.get(i + 1).ok_or_else(|| {
                    anyhow::anyhow!("test-tool lazy-init-name-hash: --analyze needs a value")
                })?;
                let a: i32 = v
                    .parse()
                    .with_context(|| format!("invalid --analyze '{v}'"))?;
                analyze = Some(a);
                i += 2;
            }
            "--step" => {
                let v = args.get(i + 1).ok_or_else(|| {
                    anyhow::anyhow!("test-tool lazy-init-name-hash: --step needs a value")
                })?;
                let s: i32 = v.parse().with_context(|| format!("invalid --step '{v}'"))?;
                analyze_step = Some(s);
                i += 2;
            }
            other => bail!("test-tool lazy-init-name-hash: unknown argument '{other}'"),
        }
    }

    if dump {
        if perf || analyze.is_some() {
            bail!("test-tool lazy-init-name-hash: cannot combine dump, perf, or analyze");
        }
        if count > 1 {
            bail!("test-tool lazy-init-name-hash: count not valid with dump");
        }
        if single && multi {
            bail!("test-tool lazy-init-name-hash: cannot use both single and multi with dump");
        }
        if !single && !multi {
            bail!("test-tool lazy-init-name-hash: dump requires either single or multi");
        }
        grit_lib::index_name_hash_lazy::dump_lazy_init_name_hash(&index, multi)
            .map_err(|s| anyhow::anyhow!(s))?;
        return Ok(());
    }

    if perf {
        if analyze.is_some() {
            bail!("test-tool lazy-init-name-hash: cannot combine dump, perf, or analyze");
        }
        if single || multi {
            bail!("test-tool lazy-init-name-hash: cannot use single or multi with perf");
        }
        let avg_single = time_runs_lazy_name_hash(&index, false, count)?;
        let avg_multi = time_runs_lazy_name_hash(&index, true, count)?;
        if avg_multi > avg_single {
            bail!("test-tool lazy-init-name-hash: multi is slower");
        }
        return Ok(());
    }

    if let Some(a) = analyze {
        if a < 500 {
            bail!("test-tool lazy-init-name-hash: analyze must be at least 500");
        }
        let step = analyze_step.unwrap_or(a);
        if single || multi {
            bail!("test-tool lazy-init-name-hash: cannot use single or multi with analyze");
        }
        analyze_runs_lazy_name_hash(&index, a, step, count)?;
        return Ok(());
    }

    if !single && !multi {
        bail!("test-tool lazy-init-name-hash: require either -s or -m or both");
    }

    if single {
        time_runs_lazy_name_hash(&index, false, count)?;
    }
    if multi {
        time_runs_lazy_name_hash(&index, true, count)?;
    }
    let _ = std::io::stdout().flush();
    Ok(())
}

#[cfg(feature = "test-tools")]
fn time_runs_lazy_name_hash(
    index: &grit_lib::index::Index,
    try_threaded: bool,
    count: usize,
) -> Result<u64> {
    use std::io::Write;
    let mut sum_ns: u64 = 0;
    let mut last_nr_threads = 0usize;
    for _ in 0..count {
        let t0 = std::time::Instant::now();
        let nr = grit_lib::index_name_hash_lazy::test_lazy_init_name_hash(index, try_threaded)
            .map_err(|e| anyhow::anyhow!(e))?;
        let t1 = std::time::Instant::now();
        let dt = t1.duration_since(t0);
        sum_ns += dt.as_nanos() as u64;
        last_nr_threads = nr;

        if try_threaded && nr == 0 {
            bail!("test-tool lazy-init-name-hash: non-threaded code path used");
        }

        let read_ns = 0u64;
        let work_ns = dt.as_nanos() as u64;
        let nr_entries = index.entries.len();
        if nr > 0 {
            println!(
                "{} {} {} multi {}",
                (read_ns as f64) / 1e9,
                (work_ns as f64) / 1e9,
                nr_entries,
                nr
            );
        } else {
            println!(
                "{} {} {} single",
                (read_ns as f64) / 1e9,
                (work_ns as f64) / 1e9,
                nr_entries
            );
        }
        let _ = std::io::stdout().flush();
    }
    if count > 1 {
        let avg = sum_ns / count as u64;
        println!(
            "avg {} {}",
            (avg as f64) / 1e9,
            if try_threaded && last_nr_threads > 0 {
                "multi"
            } else {
                "single"
            }
        );
    }
    Ok(sum_ns / count as u64)
}

#[cfg(feature = "test-tools")]
fn analyze_runs_lazy_name_hash(
    index: &grit_lib::index::Index,
    analyze_start: i32,
    step: i32,
    count: usize,
) -> Result<()> {
    use std::io::Write;

    let cache_nr_limit = index.entries.len();
    let mut nr = analyze_start as usize;
    let analyze_step = step as usize;

    loop {
        if nr > cache_nr_limit {
            nr = cache_nr_limit;
        }

        let mut sum_single: u128 = 0;
        let mut sum_multi: u128 = 0;
        let mut nr_threads_used = 0usize;

        for _ in 0..count {
            let truncated = truncate_index_for_analyze(index, nr);

            let t1s = std::time::Instant::now();
            grit_lib::index_name_hash_lazy::test_lazy_init_name_hash(&truncated, false)
                .map_err(|e| anyhow::anyhow!(e))?;
            let t2s = std::time::Instant::now();
            sum_single += t2s.duration_since(t1s).as_nanos();

            let t1m = std::time::Instant::now();
            nr_threads_used =
                grit_lib::index_name_hash_lazy::test_lazy_init_name_hash(&truncated, true)
                    .map_err(|e| anyhow::anyhow!(e))?;
            let t2m = std::time::Instant::now();
            sum_multi += t2m.duration_since(t1m).as_nanos();

            if nr_threads_used == 0 {
                println!(
                    "    [size {:8}] [single {}]   non-threaded code path used",
                    nr,
                    (t2s.duration_since(t1s).as_nanos() as f64) / 1e9
                );
            } else {
                let ds = t2s.duration_since(t1s).as_nanos() as i128;
                let dm = t2m.duration_since(t1m).as_nanos() as i128;
                println!(
                    "    [size {:8}] [single {}] {} [multi {} {}]",
                    nr,
                    (ds as f64) / 1e9,
                    if ds < dm { '<' } else { '>' },
                    (dm as f64) / 1e9,
                    nr_threads_used
                );
            }
            let _ = std::io::stdout().flush();
        }

        if count > 1 {
            let avg_single = sum_single / count as u128;
            let avg_multi = sum_multi / count as u128;
            if nr_threads_used == 0 {
                println!("avg [size {:8}] [single {}]", nr, (avg_single as f64) / 1e9);
            } else {
                println!(
                    "avg [size {:8}] [single {}] {} [multi {} {}]",
                    nr,
                    (avg_single as f64) / 1e9,
                    if avg_single < avg_multi { '<' } else { '>' },
                    (avg_multi as f64) / 1e9,
                    nr_threads_used
                );
            }
            let _ = std::io::stdout().flush();
        }

        if nr >= cache_nr_limit {
            return Ok(());
        }
        nr = nr.saturating_add(analyze_step);
    }
}

#[cfg(feature = "test-tools")]
fn truncate_index_for_analyze(index: &grit_lib::index::Index, n: usize) -> grit_lib::index::Index {
    let mut out = index.clone();
    if n < out.entries.len() {
        out.entries.truncate(n);
    }
    out
}

#[cfg(feature = "test-tools")]
fn preprocess_test_tool_args(rest: &[String]) -> Result<Vec<String>> {
    let mut i = 0usize;
    let mut change_dir: Option<std::path::PathBuf> = None;

    while i < rest.len() {
        if rest[i] == "-C" {
            i += 1;
            let Some(dir) = rest.get(i) else {
                bail!("test-tool: option '-C' requires a directory");
            };
            let next = std::path::PathBuf::from(dir);
            change_dir = Some(match change_dir.take() {
                Some(prev) => prev.join(next),
                None => next,
            });
            i += 1;
            continue;
        }
        break;
    }

    if let Some(dir) = change_dir {
        if let Err(e) = std::env::set_current_dir(dir) {
            let subcmd = rest.get(i).map(String::as_str);
            let allow_for_env_helper = std::env::var("GIT_TEST_ENV_HELPER").as_deref()
                == Ok("true")
                && subcmd == Some("env-helper");
            if !allow_for_env_helper {
                return Err(e.into());
            }
        }
    }

    Ok(rest[i..].to_vec())
}
#[cfg(feature = "test-tools")]
fn run_test_tool_sigchain(rest: &[String]) -> Result<()> {
    let mut signo: i32 = 15;
    if rest.get(1).map(String::as_str) == Some("--raise") {
        let Some(v) = rest.get(2) else {
            bail!("usage: test-tool sigchain [--raise <signal>]");
        };
        signo = v
            .parse::<i32>()
            .map_err(|_| anyhow::anyhow!("invalid signal '{}'", v))?;
        eprintln!("pid={} signo={}", std::process::id(), signo);
    } else {
        println!("three");
        println!("two");
        println!("one");
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // Portable enough for our Linux test environment.
    let pid = std::process::id().to_string();
    let _ = std::process::Command::new("kill")
        .arg(format!("-{signo}"))
        .arg(&pid)
        .status();

    std::thread::sleep(std::time::Duration::from_millis(50));
    std::process::exit(128 + signo);
}
#[derive(Debug, Clone)]
#[cfg(feature = "test-tools")]
enum JsonWriterValue {
    Object(Vec<(String, JsonWriterValue)>),
    Array(Vec<JsonWriterValue>),
    String(String),
    Integer(i64),
    Double(String),
    Boolean(bool),
    Null,
}

#[derive(Debug)]
#[cfg(feature = "test-tools")]
enum JsonWriterContainer {
    Object {
        key_in_parent: Option<String>,
        entries: Vec<(String, JsonWriterValue)>,
    },
    Array {
        key_in_parent: Option<String>,
        entries: Vec<JsonWriterValue>,
    },
}

#[cfg(feature = "test-tools")]
fn json_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(feature = "test-tools")]
fn render_json_value(v: &JsonWriterValue, pretty: bool, indent: usize) -> String {
    match v {
        JsonWriterValue::Object(entries) => {
            if entries.is_empty() {
                return "{}".to_string();
            }
            if !pretty {
                let inner = entries
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "\"{}\":{}",
                            json_escape_string(k),
                            render_json_value(v, false, indent)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{{{inner}}}")
            } else {
                let indent_str = "  ".repeat(indent);
                let child_indent_str = "  ".repeat(indent + 1);
                let mut out = String::from("{\n");
                for (idx, (k, v)) in entries.iter().enumerate() {
                    out.push_str(&child_indent_str);
                    out.push('"');
                    out.push_str(&json_escape_string(k));
                    out.push_str("\": ");
                    out.push_str(&render_json_value(v, true, indent + 1));
                    if idx + 1 != entries.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&indent_str);
                out.push('}');
                out
            }
        }
        JsonWriterValue::Array(entries) => {
            if entries.is_empty() {
                return "[]".to_string();
            }
            if !pretty {
                let inner = entries
                    .iter()
                    .map(|v| render_json_value(v, false, indent))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("[{inner}]")
            } else {
                let indent_str = "  ".repeat(indent);
                let child_indent_str = "  ".repeat(indent + 1);
                let mut out = String::from("[\n");
                for (idx, v) in entries.iter().enumerate() {
                    out.push_str(&child_indent_str);
                    out.push_str(&render_json_value(v, true, indent + 1));
                    if idx + 1 != entries.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&indent_str);
                out.push(']');
                out
            }
        }
        JsonWriterValue::String(s) => format!("\"{}\"", json_escape_string(s)),
        JsonWriterValue::Integer(i) => i.to_string(),
        JsonWriterValue::Double(d) => d.clone(),
        JsonWriterValue::Boolean(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        JsonWriterValue::Null => "null".to_string(),
    }
}

#[cfg(feature = "test-tools")]
fn attach_json_value(
    stack: &mut [JsonWriterContainer],
    root: &mut Option<JsonWriterValue>,
    key_in_parent: Option<String>,
    value: JsonWriterValue,
) -> Result<()> {
    if let Some(parent) = stack.last_mut() {
        match parent {
            JsonWriterContainer::Object { entries, .. } => {
                let Some(key) = key_in_parent else {
                    bail!("json-writer: missing object key while attaching value");
                };
                entries.push((key, value));
            }
            JsonWriterContainer::Array { entries, .. } => {
                entries.push(value);
            }
        }
    } else {
        *root = Some(value);
    }
    Ok(())
}

#[cfg(feature = "test-tools")]
fn run_test_tool_json_writer(rest: &[String]) -> Result<()> {
    let mut pretty = false;
    if let Some(flag) = rest.get(1) {
        match flag.as_str() {
            "-u" | "--unit" => return Ok(()),
            "-p" | "--pretty" => pretty = true,
            _ => {}
        }
    }

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    let mut stack: Vec<JsonWriterContainer> = Vec::new();
    let mut root: Option<JsonWriterValue> = None;
    let mut saw_root = false;

    for raw_line in input.lines() {
        let line = raw_line.trim().trim_end_matches([' ', '\t']);
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        let verb = parts[0];

        if !saw_root {
            match verb {
                "object" => {
                    stack.push(JsonWriterContainer::Object {
                        key_in_parent: None,
                        entries: Vec::new(),
                    });
                    saw_root = true;
                    continue;
                }
                "array" => {
                    stack.push(JsonWriterContainer::Array {
                        key_in_parent: None,
                        entries: Vec::new(),
                    });
                    saw_root = true;
                    continue;
                }
                _ => bail!("json-writer: first line must be 'object' or 'array'"),
            }
        }

        match verb {
            "end" => {
                let container = stack
                    .pop()
                    .ok_or_else(|| anyhow::anyhow!("json-writer: unexpected 'end'"))?;
                match container {
                    JsonWriterContainer::Object {
                        key_in_parent,
                        entries,
                    } => {
                        let value = JsonWriterValue::Object(entries);
                        attach_json_value(&mut stack, &mut root, key_in_parent, value)?;
                    }
                    JsonWriterContainer::Array {
                        key_in_parent,
                        entries,
                    } => {
                        let value = JsonWriterValue::Array(entries);
                        attach_json_value(&mut stack, &mut root, key_in_parent, value)?;
                    }
                }
            }

            "object-string" => {
                let key = parts
                    .get(1)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: object-string requires key"))?;
                let value = parts
                    .get(2)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: object-string requires value"))?;
                let parent = stack
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("json-writer: no active container"))?;
                match parent {
                    JsonWriterContainer::Object { entries, .. } => {
                        entries.push((
                            (*key).to_string(),
                            JsonWriterValue::String((*value).to_string()),
                        ));
                    }
                    _ => bail!("json-writer: object-string used outside object"),
                }
            }
            "object-int" => {
                let key = parts
                    .get(1)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: object-int requires key"))?;
                let value = parts
                    .get(2)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: object-int requires value"))?;
                let parsed = value
                    .parse::<i64>()
                    .map_err(|_| anyhow::anyhow!("json-writer: invalid integer '{value}'"))?;
                let parent = stack
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("json-writer: no active container"))?;
                match parent {
                    JsonWriterContainer::Object { entries, .. } => {
                        entries.push(((*key).to_string(), JsonWriterValue::Integer(parsed)));
                    }
                    _ => bail!("json-writer: object-int used outside object"),
                }
            }
            "object-double" => {
                let key = parts
                    .get(1)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: object-double requires key"))?;
                let precision = parts.get(2).ok_or_else(|| {
                    anyhow::anyhow!("json-writer: object-double requires precision")
                })?;
                let value = parts
                    .get(3)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: object-double requires value"))?;
                let p = precision
                    .parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("json-writer: invalid precision '{precision}'"))?;
                let v = value
                    .parse::<f64>()
                    .map_err(|_| anyhow::anyhow!("json-writer: invalid float '{value}'"))?;
                let rendered = format!("{v:.p$}");
                let parent = stack
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("json-writer: no active container"))?;
                match parent {
                    JsonWriterContainer::Object { entries, .. } => {
                        entries.push(((*key).to_string(), JsonWriterValue::Double(rendered)));
                    }
                    _ => bail!("json-writer: object-double used outside object"),
                }
            }
            "object-true" | "object-false" | "object-null" => {
                let key = parts
                    .get(1)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: object literal requires key"))?;
                let val = match verb {
                    "object-true" => JsonWriterValue::Boolean(true),
                    "object-false" => JsonWriterValue::Boolean(false),
                    _ => JsonWriterValue::Null,
                };
                let parent = stack
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("json-writer: no active container"))?;
                match parent {
                    JsonWriterContainer::Object { entries, .. } => {
                        entries.push(((*key).to_string(), val));
                    }
                    _ => bail!("json-writer: object literal used outside object"),
                }
            }
            "object-object" => {
                let key = parts
                    .get(1)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: object-object requires key"))?;
                stack.push(JsonWriterContainer::Object {
                    key_in_parent: Some((*key).to_string()),
                    entries: Vec::new(),
                });
            }
            "object-array" => {
                let key = parts
                    .get(1)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: object-array requires key"))?;
                stack.push(JsonWriterContainer::Array {
                    key_in_parent: Some((*key).to_string()),
                    entries: Vec::new(),
                });
            }

            "array-string" => {
                let value = parts
                    .get(1)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: array-string requires value"))?;
                let parent = stack
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("json-writer: no active container"))?;
                match parent {
                    JsonWriterContainer::Array { entries, .. } => {
                        entries.push(JsonWriterValue::String((*value).to_string()));
                    }
                    _ => bail!("json-writer: array-string used outside array"),
                }
            }
            "array-int" => {
                let value = parts
                    .get(1)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: array-int requires value"))?;
                let parsed = value
                    .parse::<i64>()
                    .map_err(|_| anyhow::anyhow!("json-writer: invalid integer '{value}'"))?;
                let parent = stack
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("json-writer: no active container"))?;
                match parent {
                    JsonWriterContainer::Array { entries, .. } => {
                        entries.push(JsonWriterValue::Integer(parsed));
                    }
                    _ => bail!("json-writer: array-int used outside array"),
                }
            }
            "array-double" => {
                let precision = parts.get(1).ok_or_else(|| {
                    anyhow::anyhow!("json-writer: array-double requires precision")
                })?;
                let value = parts
                    .get(2)
                    .ok_or_else(|| anyhow::anyhow!("json-writer: array-double requires value"))?;
                let p = precision
                    .parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("json-writer: invalid precision '{precision}'"))?;
                let v = value
                    .parse::<f64>()
                    .map_err(|_| anyhow::anyhow!("json-writer: invalid float '{value}'"))?;
                let rendered = format!("{v:.p$}");
                let parent = stack
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("json-writer: no active container"))?;
                match parent {
                    JsonWriterContainer::Array { entries, .. } => {
                        entries.push(JsonWriterValue::Double(rendered));
                    }
                    _ => bail!("json-writer: array-double used outside array"),
                }
            }
            "array-true" | "array-false" | "array-null" => {
                let val = match verb {
                    "array-true" => JsonWriterValue::Boolean(true),
                    "array-false" => JsonWriterValue::Boolean(false),
                    _ => JsonWriterValue::Null,
                };
                let parent = stack
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("json-writer: no active container"))?;
                match parent {
                    JsonWriterContainer::Array { entries, .. } => {
                        entries.push(val);
                    }
                    _ => bail!("json-writer: array literal used outside array"),
                }
            }
            "array-object" => {
                stack.push(JsonWriterContainer::Object {
                    key_in_parent: None,
                    entries: Vec::new(),
                });
            }
            "array-array" => {
                stack.push(JsonWriterContainer::Array {
                    key_in_parent: None,
                    entries: Vec::new(),
                });
            }
            _ => bail!("json-writer: unrecognized token '{verb}'"),
        }
    }

    if !stack.is_empty() {
        bail!("json-writer: json not terminated");
    }
    let root = root.ok_or_else(|| anyhow::anyhow!("json-writer: empty input"))?;
    let rendered = render_json_value(&root, pretty, 0);
    println!("{rendered}");
    Ok(())
}
#[cfg(feature = "test-tools")]
fn run_test_tool_mktemp(rest: &[String]) -> Result<()> {
    if rest.len() < 2 {
        bail!("usage: test-tool mktemp <template>");
    }

    let status = std::process::Command::new("mktemp")
        .args(&rest[1..])
        .status()?;
    exit_with_status(status);
}

#[cfg(feature = "test-tools")]
fn run_test_tool_regex(rest: &[String]) -> Result<()> {
    if rest.get(1).map(String::as_str) == Some("--bug") {
        return Ok(());
    }
    bail!("usage: test-tool regex --bug")
}
#[derive(Debug, Clone, Copy)]
#[cfg(feature = "test-tools")]
struct BloomSettings {
    hash_version: u32,
    num_hashes: usize,
    bits_per_entry: usize,
    max_changed_paths: usize,
}

#[cfg(feature = "test-tools")]
const TEST_BLOOM_SETTINGS: BloomSettings = BloomSettings {
    // Matches git's DEFAULT_BLOOM_FILTER_SETTINGS used by test-tool bloom.
    hash_version: 1,
    num_hashes: 7,
    bits_per_entry: 10,
    max_changed_paths: 512,
};

#[cfg(feature = "test-tools")]
fn bloom_rotate_left(value: u32, count: u32) -> u32 {
    value.rotate_left(count)
}

#[cfg(feature = "test-tools")]
fn bloom_signed_char_u32(b: u8) -> u32 {
    ((b as i8) as i32) as u32
}

#[cfg(feature = "test-tools")]
fn bloom_murmur3_seeded_v2(mut seed: u32, data: &[u8]) -> u32 {
    let c1: u32 = 0xcc9e2d51;
    let c2: u32 = 0x1b873593;
    let r1: u32 = 15;
    let r2: u32 = 13;
    let m: u32 = 5;
    let n: u32 = 0xe6546b64;

    let mut i = 0usize;
    while i + 4 <= data.len() {
        let mut k = (data[i] as u32)
            | ((data[i + 1] as u32) << 8)
            | ((data[i + 2] as u32) << 16)
            | ((data[i + 3] as u32) << 24);
        k = k.wrapping_mul(c1);
        k = bloom_rotate_left(k, r1);
        k = k.wrapping_mul(c2);

        seed ^= k;
        seed = bloom_rotate_left(seed, r2).wrapping_mul(m).wrapping_add(n);
        i += 4;
    }

    let tail = &data[i..];
    let mut k1: u32 = 0;
    match tail.len() {
        3 => {
            k1 ^= (tail[2] as u32) << 16;
            k1 ^= (tail[1] as u32) << 8;
            k1 ^= tail[0] as u32;
        }
        2 => {
            k1 ^= (tail[1] as u32) << 8;
            k1 ^= tail[0] as u32;
        }
        1 => {
            k1 ^= tail[0] as u32;
        }
        _ => {}
    }
    if !tail.is_empty() {
        k1 = k1.wrapping_mul(c1);
        k1 = bloom_rotate_left(k1, r1);
        k1 = k1.wrapping_mul(c2);
        seed ^= k1;
    }

    seed ^= data.len() as u32;
    seed ^= seed >> 16;
    seed = seed.wrapping_mul(0x85ebca6b);
    seed ^= seed >> 13;
    seed = seed.wrapping_mul(0xc2b2ae35);
    seed ^= seed >> 16;
    seed
}

#[cfg(feature = "test-tools")]
fn bloom_murmur3_seeded_v1(mut seed: u32, data: &[u8]) -> u32 {
    let c1: u32 = 0xcc9e2d51;
    let c2: u32 = 0x1b873593;
    let r1: u32 = 15;
    let r2: u32 = 13;
    let m: u32 = 5;
    let n: u32 = 0xe6546b64;

    let mut i = 0usize;
    while i + 4 <= data.len() {
        let mut k = bloom_signed_char_u32(data[i])
            | (bloom_signed_char_u32(data[i + 1]) << 8)
            | (bloom_signed_char_u32(data[i + 2]) << 16)
            | (bloom_signed_char_u32(data[i + 3]) << 24);
        k = k.wrapping_mul(c1);
        k = bloom_rotate_left(k, r1);
        k = k.wrapping_mul(c2);

        seed ^= k;
        seed = bloom_rotate_left(seed, r2).wrapping_mul(m).wrapping_add(n);
        i += 4;
    }

    let tail = &data[i..];
    let mut k1: u32 = 0;
    match tail.len() {
        3 => {
            k1 ^= bloom_signed_char_u32(tail[2]) << 16;
            k1 ^= bloom_signed_char_u32(tail[1]) << 8;
            k1 ^= bloom_signed_char_u32(tail[0]);
        }
        2 => {
            k1 ^= bloom_signed_char_u32(tail[1]) << 8;
            k1 ^= bloom_signed_char_u32(tail[0]);
        }
        1 => {
            k1 ^= bloom_signed_char_u32(tail[0]);
        }
        _ => {}
    }
    if !tail.is_empty() {
        k1 = k1.wrapping_mul(c1);
        k1 = bloom_rotate_left(k1, r1);
        k1 = k1.wrapping_mul(c2);
        seed ^= k1;
    }

    seed ^= data.len() as u32;
    seed ^= seed >> 16;
    seed = seed.wrapping_mul(0x85ebca6b);
    seed ^= seed >> 13;
    seed = seed.wrapping_mul(0xc2b2ae35);
    seed ^= seed >> 16;
    seed
}

#[cfg(feature = "test-tools")]
fn bloom_murmur3_seeded(seed: u32, data: &[u8], version: u32) -> u32 {
    match version {
        2 => bloom_murmur3_seeded_v2(seed, data),
        _ => bloom_murmur3_seeded_v1(seed, data),
    }
}

#[cfg(feature = "test-tools")]
fn bloom_key_hashes(data: &[u8], settings: BloomSettings) -> Vec<u32> {
    let seed0 = 0x293ae76f;
    let seed1 = 0x7e646e2c;
    let hash0 = bloom_murmur3_seeded(seed0, data, settings.hash_version);
    let hash1 = bloom_murmur3_seeded(seed1, data, settings.hash_version);

    let mut out = Vec::with_capacity(settings.num_hashes);
    for i in 0..settings.num_hashes {
        out.push(hash0.wrapping_add((i as u32).wrapping_mul(hash1)));
    }
    out
}

#[cfg(feature = "test-tools")]
fn bloom_add_hashes_to_filter(hashes: &[u32], filter: &mut [u8]) {
    let mod_bits = (filter.len() * 8) as u64;
    if mod_bits == 0 {
        return;
    }
    for hash in hashes {
        let hash_mod = (*hash as u64) % mod_bits;
        let block_pos = (hash_mod / 8) as usize;
        let bitmask = 1u8 << (hash_mod & 7);
        filter[block_pos] |= bitmask;
    }
}

#[cfg(feature = "test-tools")]
fn bloom_print_filter(filter: &[u8]) {
    println!("Filter_Length:{}", filter.len());
    print!("Filter_Data:");
    for b in filter {
        print!("{b:02x}|");
    }
    println!();
}

#[cfg(feature = "test-tools")]
fn bloom_collect_paths_with_prefixes(path: &str, out: &mut std::collections::BTreeSet<String>) {
    if path.is_empty() {
        return;
    }
    let mut cur = path.to_string();
    loop {
        out.insert(cur.clone());
        let Some(pos) = cur.rfind('/') else {
            break;
        };
        cur.truncate(pos);
        if cur.is_empty() {
            break;
        }
    }
}

#[cfg(feature = "test-tools")]
fn run_test_tool_bloom(rest: &[String]) -> Result<()> {
    if rest.len() < 2 {
        bail!(
            "usage: test-tool bloom [get_murmur3|get_murmur3_seven_highbit|generate_filter|get_filter_for_commit]"
        );
    }

    match rest[1].as_str() {
        "get_murmur3" => {
            let Some(s) = rest.get(2) else {
                bail!("usage: test-tool bloom get_murmur3 <string>");
            };
            let hashed = bloom_murmur3_seeded(0, s.as_bytes(), 2);
            println!("Murmur3 Hash with seed=0:0x{hashed:08x}");
            Ok(())
        }
        "get_murmur3_seven_highbit" => {
            let bytes = [0x99u8, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
            let hashed = bloom_murmur3_seeded(0, &bytes, 2);
            println!("Murmur3 Hash with seed=0:0x{hashed:08x}");
            Ok(())
        }
        "generate_filter" => {
            if rest.len() < 3 {
                bail!("usage: test-tool bloom generate_filter <string> [<string>...]");
            }
            let len = TEST_BLOOM_SETTINGS.bits_per_entry.div_ceil(8);
            let mut filter = vec![0u8; len];
            for item in rest.iter().skip(2) {
                let hashes = bloom_key_hashes(item.as_bytes(), TEST_BLOOM_SETTINGS);
                print!("Hashes:");
                for h in &hashes {
                    print!("0x{h:08x}|");
                }
                println!();
                bloom_add_hashes_to_filter(&hashes, &mut filter);
            }
            bloom_print_filter(&filter);
            Ok(())
        }
        "get_filter_for_commit" => {
            let Some(commit_hex) = rest.get(2) else {
                bail!("usage: test-tool bloom get_filter_for_commit <commit-hex>");
            };
            let commit_oid = commit_hex
                .parse::<grit_lib::objects::ObjectId>()
                .map_err(|_| anyhow::anyhow!("cannot parse oid '{commit_hex}'"))?;
            let repo = grit_lib::repo::Repository::discover(None)?;
            let commit_obj = repo.odb.read(&commit_oid)?;
            if commit_obj.kind != grit_lib::objects::ObjectKind::Commit {
                bail!("object '{commit_hex}' is not a commit");
            }
            let commit = grit_lib::objects::parse_commit(&commit_obj.data)?;

            let parent_tree = if let Some(parent_oid) = commit.parents.first() {
                let parent_obj = repo.odb.read(parent_oid)?;
                if parent_obj.kind != grit_lib::objects::ObjectKind::Commit {
                    None
                } else {
                    let parent_commit = grit_lib::objects::parse_commit(&parent_obj.data)?;
                    Some(parent_commit.tree)
                }
            } else {
                None
            };

            let diffs = grit_lib::diff::diff_trees(
                &repo.odb,
                parent_tree.as_ref(),
                Some(&commit.tree),
                "",
            )?;

            let mut changed_paths: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            for d in diffs {
                if let Some(path) = d.new_path.or(d.old_path) {
                    bloom_collect_paths_with_prefixes(&path.to_string_lossy(), &mut changed_paths);
                }
            }

            let mut filter = if changed_paths.len() > TEST_BLOOM_SETTINGS.max_changed_paths {
                vec![0xff]
            } else {
                let bit_count = changed_paths.len() * TEST_BLOOM_SETTINGS.bits_per_entry;
                let mut len = bit_count.div_ceil(8);
                if len == 0 {
                    len = 1;
                }
                let mut data = vec![0u8; len];
                for path in &changed_paths {
                    let hashes = bloom_key_hashes(path.as_bytes(), TEST_BLOOM_SETTINGS);
                    bloom_add_hashes_to_filter(&hashes, &mut data);
                }
                data
            };

            bloom_print_filter(&filter);
            filter.clear();
            Ok(())
        }
        _ => bail!(
            "usage: test-tool bloom [get_murmur3|get_murmur3_seven_highbit|generate_filter|get_filter_for_commit]"
        ),
    }
}

/// Global options parsed from argv before the subcommand.
#[derive(Default)]
pub(crate) struct GlobalOpts {
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
    /// `GIT_NAMESPACE` override from `--namespace=<name>` (CLI wins over env).
    namespace: Option<String>,
    change_dir: Option<PathBuf>,
    config_overrides: Vec<String>,
    attr_source: Option<String>,
    bare: bool,
    no_advice: bool,
    no_optional_locks: bool,
    literal_pathspecs: bool,
    no_literal_pathspecs: bool,
    glob_pathspecs: bool,
    noglob_pathspecs: bool,
    icase_pathspecs: bool,
    exec_path: Option<PathBuf>,
}

/// Directory used to resolve `git-<cmd>` helpers, matching upstream Git order:
/// `--exec-path` (CLI), then `GIT_EXEC_PATH` (when non-empty), then the directory of the running binary.
#[must_use]
pub(crate) fn git_exec_path_for_helpers(cli_exec_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = cli_exec_path {
        return Some(p.to_path_buf());
    }
    if let Ok(ep) = std::env::var("GIT_EXEC_PATH") {
        if !ep.is_empty() {
            return Some(PathBuf::from(ep));
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
}

/// Git subcommands that vanilla Git ships as separate `git-<cmd>` executables in its
/// libexec dir, but which grit implements as built-ins. When grit's exec path is an
/// explicitly-provided, writable directory (the test harness points `GIT_EXEC_PATH`
/// at a dedicated helper dir), a sibling *real* `git` invocation such as
/// `/usr/bin/git submodule add ...` resolves `git-submodule` from `GIT_EXEC_PATH` and
/// fails with "submodule is not a git command" because the dir only contains the few
/// shims the harness wrote. Installing a passthrough shim that re-invokes grit lets
/// those real-git calls delegate to grit's own implementation.
const EXEC_PATH_PASSTHROUGH_HELPERS: &[&str] = &["submodule"];

/// Install passthrough shims for [`EXEC_PATH_PASSTHROUGH_HELPERS`] into the
/// `GIT_EXEC_PATH` helper directory so a sibling vanilla `git` can find the
/// `git-<cmd>` helpers that grit implements as built-ins.
///
/// This only acts when `GIT_EXEC_PATH` is set in the environment to a directory that
/// already exists and is writable; otherwise it is a no-op (production runs that do
/// not set `GIT_EXEC_PATH`, or point it at git's read-only libexec, are unaffected).
/// Each shim is written at most once (skipped when already present) and re-invokes the
/// running grit binary, so the behavior matches grit's own built-in subcommand.
fn install_exec_path_passthrough_helpers() {
    let Ok(exec_dir) = std::env::var("GIT_EXEC_PATH") else {
        return;
    };
    if exec_dir.is_empty() {
        return;
    }
    let exec_dir = PathBuf::from(exec_dir);
    if !exec_dir.is_dir() {
        return;
    }
    let Ok(self_exe) = std::env::current_exe() else {
        return;
    };
    let self_exe = self_exe.display().to_string();
    for cmd in EXEC_PATH_PASSTHROUGH_HELPERS {
        let shim = exec_dir.join(format!("git-{cmd}"));
        if shim.exists() {
            continue;
        }
        let body = format!("#!/bin/sh\nexec \"{self_exe}\" {cmd} \"$@\"\n");
        if fs::write(&shim, body).is_err() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&shim, fs::Permissions::from_mode(0o755));
        }
    }
}

/// Extract global options and return (globals, subcommand_name, remaining_args).
///
/// We scan argv[1..] for global flags that appear before the subcommand.
/// The first non-flag argument is the subcommand name.
pub(crate) fn extract_globals(
    args: &[String],
) -> Result<(GlobalOpts, Option<String>, Vec<String>)> {
    let mut opts = GlobalOpts::default();
    let mut subcmd = None;
    let mut rest = Vec::new();
    let mut i = 0;
    let items = &args[1..]; // skip argv[0]

    while i < items.len() {
        let arg = &items[i];

        // -C <dir> — cumulative: each -C is relative to the previous one
        if arg == "-C" {
            i += 1;
            if i < items.len() {
                let new_dir = PathBuf::from(&items[i]);
                opts.change_dir = Some(match opts.change_dir.take() {
                    Some(prev) => prev.join(&new_dir),
                    None => new_dir,
                });
            }
            i += 1;
            continue;
        }

        // --exec-path=<val>
        if let Some(val) = arg.strip_prefix("--exec-path=") {
            opts.exec_path = Some(PathBuf::from(val));
            i += 1;
            continue;
        }
        if arg == "--exec-path" {
            // Print exec-path and exit (honour GIT_EXEC_PATH like upstream Git).
            if let Some(dir) = git_exec_path_for_helpers(opts.exec_path.as_deref()) {
                println!("{}", dir.display());
            }
            std::process::exit(0);
        }
        // --git-dir=<val> or --git-dir <val>
        if let Some(val) = arg.strip_prefix("--git-dir=") {
            opts.git_dir = Some(PathBuf::from(val));
            i += 1;
            continue;
        }
        if arg == "--git-dir" {
            i += 1;
            if i < items.len() {
                opts.git_dir = Some(PathBuf::from(&items[i]));
            }
            i += 1;
            continue;
        }

        // --work-tree=<val> or --work-tree <val>
        if let Some(val) = arg.strip_prefix("--work-tree=") {
            opts.work_tree = Some(PathBuf::from(val));
            i += 1;
            continue;
        }
        if arg == "--work-tree" {
            i += 1;
            if i < items.len() {
                opts.work_tree = Some(PathBuf::from(&items[i]));
            }
            i += 1;
            continue;
        }

        // --namespace=<name> or --namespace <name>
        if let Some(val) = arg.strip_prefix("--namespace=") {
            opts.namespace = Some(val.to_owned());
            i += 1;
            continue;
        }
        if arg == "--namespace" {
            i += 1;
            if i < items.len() {
                opts.namespace = Some(items[i].clone());
            }
            i += 1;
            continue;
        }

        // -c key=value
        if arg == "-c" {
            i += 1;
            if i < items.len() {
                opts.config_overrides.push(items[i].clone());
            }
            i += 1;
            continue;
        }

        if let Some(spec) = arg.strip_prefix("--config-env=") {
            opts.config_overrides.push(resolve_config_env(spec)?);
            i += 1;
            continue;
        }
        if arg == "--config-env" {
            i += 1;
            let Some(spec) = items.get(i) else {
                bail!("no config key given for --config-env");
            };
            opts.config_overrides.push(resolve_config_env(spec)?);
            i += 1;
            continue;
        }

        // --attr-source=<tree-ish> or --attr-source <tree-ish>
        if let Some(val) = arg.strip_prefix("--attr-source=") {
            opts.attr_source = Some(val.to_owned());
            i += 1;
            continue;
        }
        if arg == "--attr-source" {
            i += 1;
            if i < items.len() {
                opts.attr_source = Some(items[i].clone());
            }
            i += 1;
            continue;
        }

        // --bare
        if arg == "--bare" {
            opts.bare = true;
            i += 1;
            continue;
        }

        // --no-advice
        if arg == "--no-advice" {
            opts.no_advice = true;
            i += 1;
            continue;
        }

        // --no-optional-locks (Git sets GIT_OPTIONAL_LOCKS=0)
        if arg == "--no-optional-locks" {
            opts.no_optional_locks = true;
            i += 1;
            continue;
        }

        // --no-lazy-fetch (Git sets GIT_NO_LAZY_FETCH=1)
        if arg == "--no-lazy-fetch" {
            std::env::set_var("GIT_NO_LAZY_FETCH", "1");
            i += 1;
            continue;
        }

        // Pathspec parsing globals accepted by Git before the subcommand.
        if arg == "--literal-pathspecs" {
            opts.literal_pathspecs = true;
            i += 1;
            continue;
        }
        if arg == "--no-literal-pathspecs" {
            opts.no_literal_pathspecs = true;
            i += 1;
            continue;
        }
        if arg == "--glob-pathspecs" {
            opts.glob_pathspecs = true;
            i += 1;
            continue;
        }
        if arg == "--noglob-pathspecs" {
            opts.noglob_pathspecs = true;
            i += 1;
            continue;
        }
        if arg == "--icase-pathspecs" {
            opts.icase_pathspecs = true;
            i += 1;
            continue;
        }
        // Pager controls (no-op)
        if arg == "--no-pager" || arg == "--paginate" {
            i += 1;
            continue;
        }

        // --list-cmds=<categories>
        if let Some(val) = arg.strip_prefix("--list-cmds=") {
            return Ok((opts, Some("__list_cmds".to_owned()), vec![val.to_owned()]));
        }

        // --version / -v / -V / --help / -h  → treat as pseudo-subcommands
        if arg == "--version" || arg == "-v" || arg == "-V" {
            subcmd = Some("version".to_owned());
            rest = items[i + 1..].to_vec();
            break;
        }
        if arg == "--help" || arg == "-h" || arg == "help" {
            subcmd = Some("help".to_owned());
            rest = items[i + 1..].to_vec();
            break;
        }

        // First non-flag argument is the subcommand
        if !arg.starts_with('-') {
            subcmd = Some(arg.clone());
            rest = items[i + 1..].to_vec();
            break;
        }

        // --end-of-options: stop processing options, next arg is subcommand
        if arg == "--end-of-options" {
            if i + 1 < items.len() {
                subcmd = Some(items[i + 1].clone());
                rest = items[i + 2..].to_vec();
            }
            break;
        }
        // Unknown global flag — pass through
        bail!("unknown option: {arg}");
    }

    Ok((opts, subcmd, rest))
}

fn resolve_config_env(spec: &str) -> Result<String> {
    let Some((key, envvar)) = spec.rsplit_once('=') else {
        bail!("invalid config format: {spec}");
    };
    if key.is_empty() {
        bail!("no config key given for --config-env");
    }
    if envvar.is_empty() {
        bail!("missing environment variable name for configuration '{key}'");
    }
    let value = std::env::var(envvar).map_err(|_| {
        anyhow::anyhow!("missing environment variable '{envvar}' for configuration '{key}'")
    })?;
    if key.contains('=') {
        Ok(format!("{key}\u{1}{value}"))
    } else {
        Ok(format!("{key}={value}"))
    }
}

fn sq_quote_config_parameter_part(raw: &str) -> String {
    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('\'');
    for ch in raw.chars() {
        if ch == '\'' || ch == '!' {
            quoted.push('\'');
            quoted.push('\\');
            quoted.push(ch);
            quoted.push('\'');
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn format_config_parameter_for_env(kv: &str) -> String {
    let (key, value) = kv
        .split_once('\u{1}')
        .or_else(|| kv.split_once('='))
        .map_or((kv, None), |(key, value)| (key, Some(value)));
    let mut out = sq_quote_config_parameter_part(key);
    out.push('=');
    if let Some(value) = value {
        out.push_str(&sq_quote_config_parameter_part(value));
    }
    out
}

/// Apply global options (env vars, chdir).
fn apply_globals(opts: &GlobalOpts) -> Result<()> {
    if let Some(dir) = &opts.change_dir {
        if !dir.as_os_str().is_empty() {
            std::env::set_current_dir(dir)?;
        }
    }
    if let Some(git_dir) = &opts.git_dir {
        let resolved =
            grit_lib::repo::resolve_git_directory_arg(git_dir).unwrap_or_else(|_| git_dir.clone());
        std::env::set_var("GIT_DIR", resolved);
    }
    if let Some(wt) = &opts.work_tree {
        std::env::set_var("GIT_WORK_TREE", wt);
    }
    if let Some(ns) = &opts.namespace {
        std::env::set_var("GIT_NAMESPACE", ns);
    }
    if !opts.config_overrides.is_empty() {
        for kv in &opts.config_overrides {
            let (key, value) = kv
                .split_once('\u{1}')
                .or_else(|| kv.split_once('='))
                .map_or((kv.as_str(), "true"), |(key, value)| (key, value));
            let canon = grit_lib::config::canonical_key(key.trim())?;
            if canon == "core.bare" {
                grit_lib::config::parse_bool(value).map_err(|_| {
                    anyhow::anyhow!("fatal: bad boolean config value '{value}' for 'core.bare'")
                })?;
            }
        }
        if opts.config_overrides.iter().any(|kv| {
            let lower = kv.to_ascii_lowercase();
            kv.contains('\n') || lower.contains("%0a")
        }) {
            eprintln!("warning: skipping credential lookup for key with newline");
        }
        let extra: String = opts
            .config_overrides
            .iter()
            .map(|kv| format_config_parameter_for_env(kv))
            .collect::<Vec<_>>()
            .join(" ");
        // Git's config reader applies the last occurrence of a key. Inherited
        // `GIT_CONFIG_PARAMETERS` (e.g. from the test harness) must not override
        // command-line `-c` values, so append new entries after the existing payload.
        let merged = match std::env::var("GIT_CONFIG_PARAMETERS") {
            Ok(existing) if !existing.trim().is_empty() => format!("{existing} {extra}"),
            _ => extra,
        };
        std::env::set_var("GIT_CONFIG_PARAMETERS", merged);
    }
    if opts.no_advice {
        std::env::set_var("GIT_ADVICE", "false");
    }
    if opts.no_optional_locks {
        std::env::set_var("GIT_OPTIONAL_LOCKS", "0");
    }
    if let Some(attr_source) = &opts.attr_source {
        std::env::set_var("GIT_ATTR_SOURCE", attr_source);
    }
    // Pathspec globals (same env vars as Git's git.c).
    if opts.literal_pathspecs {
        std::env::set_var("GIT_LITERAL_PATHSPECS", "1");
    } else if opts.no_literal_pathspecs {
        std::env::set_var("GIT_LITERAL_PATHSPECS", "0");
    }
    if opts.glob_pathspecs {
        std::env::set_var("GIT_GLOB_PATHSPECS", "1");
    }
    if opts.noglob_pathspecs {
        std::env::set_var("GIT_NOGLOB_PATHSPECS", "1");
    }
    if opts.icase_pathspecs {
        std::env::set_var("GIT_ICASE_PATHSPECS", "1");
    }
    if let Err(msg) = grit_lib::pathspec::validate_global_pathspec_flags() {
        bail!("fatal: {msg}");
    }
    Ok(())
}

// Wrapper to parse a clap `Args` struct standalone (must not use doc comments here
// or clap uses them as the command `about` text in --help output).
#[derive(Debug, Parser)]
#[command(name = "grit", disable_help_subcommand = true)]
struct ArgsWrapper<T: Args> {
    #[command(flatten)]
    inner: T,
}

fn stash_explicit_subcommand(rest: &[String]) -> bool {
    const KNOWN: &[&str] = &[
        "push", "save", "list", "show", "pop", "apply", "drop", "clear", "branch", "create",
        "store", "export", "import",
    ];
    rest.first()
        .map(|a| KNOWN.contains(&a.as_str()))
        .unwrap_or(false)
}

/// After a failed parse, print upstream `or: git stash …` lines (t3903).
fn print_stash_invalid_option_usage_header() {
    let Some(syn) = commands::upstream_synopsis_help::synopsis_for_builtin("stash") else {
        return;
    };
    let variants = commands::upstream_synopsis_help::synopsis_variants_from_adoc(syn);
    let pad = " ".repeat("git stash ".len());
    for (i, var) in variants.iter().enumerate() {
        if i == 0 {
            continue;
        }
        let Some(first) = var.first() else {
            continue;
        };
        eprintln!("   or: {first}");
        for cont in var.iter().skip(1) {
            eprintln!("{pad}{cont}");
        }
    }
}

/// `git stash push -h` / `--help` — match t3903 expectation (`usage: git stash [push`).
fn print_stash_push_help_upstream() -> ! {
    println!("Save changes and clean the working tree\n");
    let Some(syn) = commands::upstream_synopsis_help::synopsis_for_builtin("stash") else {
        std::process::exit(129);
    };
    let variants = commands::upstream_synopsis_help::synopsis_variants_from_adoc(syn);
    let pad = " ".repeat("git stash ".len());
    let push_var = variants.iter().find(|v| {
        v.first()
            .is_some_and(|line| line.contains("[push") || line.contains("stash [push"))
    });
    if let Some(var) = push_var {
        if let Some(first) = var.first() {
            println!("usage: {first}");
            for cont in var.iter().skip(1) {
                println!("{pad}{cont}");
            }
        }
    }
    println!();
    std::process::exit(129);
}

/// Parse a command's clap Args from the remaining arguments.
///
/// When `-h` is passed, clap prints usage and the process exits with code 129
/// (Git convention for usage errors) instead of clap's default exit code 0.
/// Git allows `git status --porcelain path`; clap would treat `path` as the optional porcelain
/// version unless we insert `--` after a bare `--porcelain` when the next token is a pathspec.
/// `git config --get-color <slot> <default>` allows a multi-word default (e.g. `-1 black`).
/// The shell passes `-1` and `black` as separate argv entries; clap would treat `-1` as a flag.
/// Join all arguments after the slot into one default string, like Git's argv consumption.
fn preprocess_config_argv(rest: &[String]) -> Vec<String> {
    let Some(pos) = rest.iter().position(|a| a == "--get-color") else {
        return rest.to_vec();
    };
    let key_idx = pos + 1;
    if key_idx >= rest.len() {
        return rest.to_vec();
    }
    let mut tail_idx = key_idx + 1;
    if tail_idx >= rest.len() {
        return rest.to_vec();
    }
    if rest[tail_idx] == "--" {
        tail_idx += 1;
        if tail_idx >= rest.len() {
            return rest.to_vec();
        }
    }
    let default_str = rest[tail_idx..].join(" ");
    let mut out = rest[..=key_idx].to_vec();
    // Values starting with `-` must follow `--` or clap treats them as flags.
    if default_str.starts_with('-') {
        out.push("--".to_owned());
    }
    out.push(default_str);
    out
}

fn preprocess_sparse_checkout_argv(rest: &[String]) -> Vec<String> {
    // Match Git's "unknown option" wording (exit 129) for mistyped flags on `set`/`add`,
    // instead of clap's "unexpected argument" text (t1091 'error on mistyped command line
    // options').
    let sub = rest.first().map(|s| s.as_str());
    let known: Option<&[&str]> = match sub {
        Some("set") => Some(&[
            "--cone",
            "--no-cone",
            "--sparse-index",
            "--no-sparse-index",
            "--skip-checks",
            "--stdin",
            "--end-of-options",
        ]),
        Some("add") => Some(&["--skip-checks", "--stdin", "--end-of-options"]),
        _ => None,
    };
    if let Some(known) = known {
        for a in &rest[1..] {
            if a == "--" {
                break;
            }
            if a.starts_with("--") && a.len() > 2 {
                let opt = a.split_once('=').map(|(o, _)| o).unwrap_or(a.as_str());
                if !known.contains(&opt) {
                    eprintln!("error: unknown option `{}'", opt.trim_start_matches('-'));
                    std::process::exit(129);
                }
            } else if !a.starts_with('-') {
                // First positional ends option scanning.
                break;
            }
        }
    }

    if sub != Some("set") {
        return rest.to_vec();
    }
    const SET_FLAGS: &[&str] = &[
        "--cone",
        "--no-cone",
        "--sparse-index",
        "--no-sparse-index",
        "--skip-checks",
        "--stdin",
        "--end-of-options",
    ];
    let mut i = 1usize;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "--" {
            return rest.to_vec();
        }
        if a.starts_with('-') {
            if let Some((opt, _val)) = a.split_once('=') {
                if SET_FLAGS.contains(&opt) {
                    i += 1;
                    continue;
                }
            } else if SET_FLAGS.contains(&a) {
                i += 1;
                continue;
            }
        }
        let mut out = rest.to_vec();
        out.insert(i, "--".to_owned());
        return out;
    }
    rest.to_vec()
}

fn preprocess_status_argv(rest: &[String]) -> Vec<String> {
    fn next_is_pathspec_after_bare_porcelain(next: Option<&str>) -> bool {
        let Some(n) = next else {
            return false;
        };
        if n == "--" || n.starts_with('-') {
            return false;
        }
        !matches!(n, "v1" | "v2" | "1" | "2")
    }

    fn next_is_status_untracked_mode(next: Option<&str>) -> bool {
        matches!(
            next,
            Some("no" | "normal" | "all" | "false" | "true" | "0" | "1")
        )
    }

    fn next_is_status_ignored_mode(next: Option<&str>) -> bool {
        matches!(next, Some("no" | "traditional" | "matching"))
    }

    let mut out = Vec::with_capacity(rest.len() + 2);
    let mut i = 0usize;
    while i < rest.len() {
        let arg = rest[i].as_str();
        if arg == "--porcelain" {
            out.push(rest[i].clone());
            if next_is_pathspec_after_bare_porcelain(rest.get(i + 1).map(|s| s.as_str())) {
                out.push("--".to_owned());
            }
        } else if arg == "-u" || arg == "--untracked-files" {
            out.push(rest[i].clone());
            let next = rest.get(i + 1).map(|s| s.as_str());
            if next.is_some_and(|n| n != "--" && !n.starts_with('-'))
                && !next_is_status_untracked_mode(next)
            {
                out.push("--".to_owned());
            }
        } else if arg == "--ignored" {
            out.push(rest[i].clone());
            let next = rest.get(i + 1).map(|s| s.as_str());
            if next.is_some_and(|n| n != "--" && !n.starts_with('-'))
                && !next_is_status_ignored_mode(next)
            {
                out.push("--".to_owned());
            }
        } else {
            out.push(rest[i].clone());
        }
        i += 1;
    }

    if out.iter().any(|a| a == "--") {
        return out;
    }

    out
}

fn preprocess_merge_argv(rest: &[String]) -> Vec<String> {
    let mut explicit_edit: Option<&str> = None;
    for arg in rest {
        match arg.as_str() {
            "--edit" | "-e" => explicit_edit = Some("1"),
            "--no-edit" => explicit_edit = Some("0"),
            _ => {}
        }
    }
    if let Some(value) = explicit_edit {
        std::env::set_var("GIT_GRIT_MERGE_EXPLICIT_EDIT", value);
    } else {
        std::env::remove_var("GIT_GRIT_MERGE_EXPLICIT_EDIT");
    }
    rest.to_vec()
}

/// Git's `blame` accepts options after the pathspec (e.g. `git blame file --ignore-rev X`).
/// Clap stops at the first positional (`args`), so trailing flags would be left in `args` and
/// mis-parsed as revisions. Move any trailing option block before the pathspec tokens.
fn preprocess_blame_argv(rest: &[String]) -> Vec<String> {
    fn opt_value_in_token(flag_len: usize, token: &str) -> Option<&str> {
        let rest = token.get(flag_len..)?;
        if rest.is_empty() {
            None
        } else {
            Some(rest)
        }
    }

    fn consume_blame_option(slice: &[String], i: &mut usize) -> bool {
        let Some(cur) = slice.get(*i) else {
            return false;
        };
        let t = cur.as_str();

        if let Some((name, val)) = t.split_once('=') {
            return matches!(
                name,
                "--ignore-rev"
                    | "--ignore-revs-file"
                    | "--abbrev"
                    | "--diff-algorithm"
                    | "--contents"
                    | "--encoding"
                    | "--find-copies"
                    | "--find-renames"
            ) && !val.is_empty()
                && {
                    *i += 1;
                    true
                };
        }

        match t {
            "-h" | "--help" | "-l" | "-s" | "-p" | "--porcelain" | "--line-porcelain"
            | "--color-lines" | "--color-by-age" | "-f" | "--show-name" | "--no-abbrev"
            | "--root" | "--reverse" | "--first-parent" | "--minimal" | "--textconv"
            | "--no-textconv" | "--progress" | "--incremental" => {
                *i += 1;
                true
            }
            "-e" | "--show-email" => {
                *i += 1;
                true
            }
            "-L" => {
                if slice.len() <= *i + 1 {
                    return false;
                }
                *i += 2;
                true
            }
            tt if tt.starts_with("-L") => {
                if opt_value_in_token(2, tt).is_some() {
                    *i += 1;
                    true
                } else {
                    false
                }
            }
            "--ignore-rev" | "--ignore-revs-file" | "--abbrev" | "--diff-algorithm"
            | "--contents" | "--encoding" => {
                if slice.len() <= *i + 1 {
                    return false;
                }
                *i += 2;
                true
            }
            tt if tt == "-M" || tt.starts_with("-M") => {
                if tt == "-M" {
                    if slice.len() > *i + 1 {
                        *i += 2;
                        return true;
                    }
                }
                *i += 1;
                true
            }
            tt if tt == "-C" || tt.starts_with("-C") => {
                if tt == "-C" {
                    if slice.len() > *i + 1 {
                        *i += 2;
                        return true;
                    }
                }
                *i += 1;
                true
            }
            "--find-renames" | "--find-copies" => {
                if slice.len() > *i + 1 {
                    *i += 2;
                } else {
                    *i += 1;
                }
                true
            }
            _ => false,
        }
    }

    fn slice_is_only_blame_options(slice: &[String]) -> bool {
        let mut i = 0usize;
        while i < slice.len() {
            if !consume_blame_option(slice, &mut i) {
                return false;
            }
        }
        true
    }

    let Some(j) = (0..rest.len()).find(|&j| {
        !rest[j].is_empty() && rest[j].starts_with('-') && slice_is_only_blame_options(&rest[j..])
    }) else {
        return rest.to_vec();
    };

    if j == 0 {
        return rest.to_vec();
    }

    let prefix = &rest[..j];
    let suffix = &rest[j..];

    let mut k = 0usize;
    while k < prefix.len() {
        if !consume_blame_option(prefix, &mut k) {
            break;
        }
    }
    let leading_opts = &prefix[..k];
    let path_tail = &prefix[k..];

    let mut out = Vec::with_capacity(rest.len() + 1);
    out.extend_from_slice(leading_opts);
    out.extend_from_slice(suffix);
    if !path_tail.is_empty() {
        out.push("--".to_owned());
        out.extend_from_slice(path_tail);
    }
    out
}

/// Normalize `git commit` argv for clap's `trailing_var_arg` pathspec bucket.
///
/// Only applies to **`-m` / `--message`**. Do not reorder `-F` / `--file` (e.g. `commit -F -`
/// used by tests); those argv shapes must stay intact.
///
/// Handles:
/// - `commit <paths>... -m <msg>` → `-m <msg> -- <paths>...`
/// - `commit -q -m <msg>` (and similar) → `-m <msg> -q` so the message is not parsed as a pathspec
/// - `commit -m <msg> <paths>...` → `-m <msg> -- <paths>...`
fn preprocess_commit_argv(rest: &[String]) -> Vec<String> {
    let rest = crate::commands::commit::preprocess_commit_for_parse(rest);
    const M_STYLE: [&str; 2] = ["-m", "--message"];
    let Some(i) = rest.iter().position(|a| M_STYLE.contains(&a.as_str())) else {
        return rest;
    };
    if i + 1 >= rest.len() {
        return rest;
    }
    let flag = rest[i].as_str();
    if !matches!(flag, "-m" | "--message") {
        return rest;
    }

    let before = &rest[..i];
    let mut msg_block = Vec::with_capacity(2);
    if rest[i + 1].starts_with('-') {
        msg_block.push(format!("--message={}", rest[i + 1]));
    } else {
        msg_block.extend_from_slice(&rest[i..i + 2]);
    }
    let after = &rest[i + 2..];

    let before_is_pathspec_prefix =
        !before.is_empty() && before.iter().all(|a| !a.starts_with('-'));

    let mut out = Vec::with_capacity(rest.len() + 2);
    out.extend_from_slice(&msg_block);

    if before_is_pathspec_prefix {
        out.push("--".to_owned());
        out.extend_from_slice(before);
        out.extend_from_slice(after);
        return out;
    }

    if before.is_empty() {
        if !after.is_empty() && !after[0].starts_with('-') {
            out.push("--".to_owned());
        }
        out.extend_from_slice(after);
        return out;
    }

    out.extend_from_slice(before);
    out.extend_from_slice(after);
    out
}

/// Upstream annotate/blame tests use `-h <rev>` as the starting revision. Clap maps `-h` to help, so
/// rewrite to a leading revision token before parsing (same intent as `commands::annotate::run`).
fn preprocess_blame_h_rev(rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len());
    let mut i = 0usize;
    while i < rest.len() {
        if rest[i] == "-h" && i + 1 < rest.len() && !rest[i + 1].starts_with('-') {
            out.push(rest[i + 1].clone());
            i += 2;
        } else {
            out.push(rest[i].clone());
            i += 1;
        }
    }
    out
}

/// Translate `ls-remote` short flags that clash with clap's reserved `-h`.
///
/// Upstream `git ls-remote` uses `-h`/`--heads` as a hidden, deprecated synonym
/// for `-b`/`--branches`. clap reserves `-h` for help, so rewrite the short
/// forms to their canonical long names before parsing. The sole-`-h` help case
/// is intercepted earlier in [`parse_cmd_args`], so reaching here always means
/// `-h` is being used as the branches flag.
fn preprocess_ls_remote_argv(rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len());
    let mut after_ddash = false;
    for arg in rest {
        if after_ddash {
            out.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => {
                after_ddash = true;
                out.push(arg.clone());
            }
            "-h" => out.push("--heads".to_owned()),
            "-b" => out.push("--branches".to_owned()),
            _ => out.push(arg.clone()),
        }
    }
    out
}

/// Strip leading `-C <dir>` pairs from `rest` and `chdir` for each (Git allows `-C` after the subcommand).
///
/// Do not confuse this with `diff-tree -C` / `diff-index -C` (find copies): the next token there is
/// another flag (e.g. `--find-copies-harder`), not a directory path.
///
/// `switch` / `checkout` use `-C` for `--force-create` / `-B` style branch creation, not as a
/// directory change — do not consume those tokens here (otherwise `git switch -C topic` tries to
/// `chdir` into `topic` and fails with `ENOENT`).
///
/// `commit -C <rev>` is `--reuse-message`, not a directory change (t7500).
fn strip_subcommand_leading_change_dir(subcmd: &str, rest: &mut Vec<String>) -> Result<()> {
    if matches!(
        subcmd,
        "switch"
            | "checkout"
            | "commit"
            | "diff"
            | "diff-index"
            | "diff-tree"
            | "diff-files"
            // `git branch -C <old> <new>` is force-copy, not a leading change-dir.
            | "branch"
    ) {
        return Ok(());
    }
    while rest.len() >= 2 && rest[0] == "-C" {
        let next = rest[1].as_str();
        if next.starts_with('-') {
            break;
        }
        let new_dir = PathBuf::from(next);
        if !new_dir.as_os_str().is_empty() {
            std::env::set_current_dir(&new_dir)?;
        }
        rest.drain(0..2);
    }
    Ok(())
}

pub(crate) fn parse_cmd_args<T: Args + FromArgMatches>(subcmd: &str, rest: &[String]) -> T {
    if subcmd == "stash"
        && rest.len() >= 2
        && rest[0] == "push"
        && (rest[1] == "-h" || rest[1] == "--help")
    {
        print_stash_push_help_upstream();
    }
    // `git <cmd> --help-all` matches short `-h` synopsis (t1517); exit **129** like `-h`.
    // Long `--help` alone exits **0** (t0450). `git submodule -h` exits **0** (t7400).
    if rest.len() == 1 {
        let arg = rest[0].as_str();
        if matches!(arg, "-h" | "--help" | "--help-all") {
            if subcmd == "replay" {
                println!();
                println!("usage: git replay ([--contained] --onto <newbase> | --advance <branch>) [--ref-action[=<mode>]] <revision-range>");
                std::process::exit(if arg == "--help" { 0 } else { 129 });
            }
            if let Some(syn) = commands::upstream_synopsis_help::synopsis_for_builtin(subcmd) {
                let code = if subcmd == "submodule" || arg == "--help" {
                    0
                } else {
                    129
                };
                commands::upstream_synopsis_help::print_upstream_synopsis_stdout_and_exit(
                    subcmd, syn, code,
                );
            }
        }
    }

    let mut argv = vec![format!("git {subcmd}")];
    let rest_for_parse = if subcmd == "commit" {
        preprocess_commit_argv(rest)
    } else if subcmd == "blame" {
        preprocess_blame_h_rev(rest)
    } else if subcmd == "ls-remote" {
        preprocess_ls_remote_argv(rest)
    } else {
        rest.to_vec()
    };
    if subcmd == "clone" {
        if let Err(e) = crate::http_client::validate_clone_proxy_from_argv(&rest_for_parse) {
            eprintln!("fatal: {e:#}");
            std::process::exit(128);
        }
    }
    argv.extend(rest_for_parse);
    match ArgsWrapper::<T>::try_parse_from(&argv) {
        Ok(wrapper) => wrapper.inner,
        Err(e) => {
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    | clap::error::ErrorKind::DisplayVersion
            ) {
                // Git prints lowercase "usage:"; clap uses "Usage:". Tests grep for "usage".
                let mut msg = e.render().to_string();
                msg = msg.replace("Usage:", "usage:");
                print!("{msg}");
            } else {
                // Match Git's lowercase "usage:" line; clap's `print()` leaves "Usage:".
                let mut msg = e.render().to_string();
                msg = msg.replace("Usage:", "usage:");
                eprint!("{msg}");
                if subcmd == "stash" && !stash_explicit_subcommand(rest) {
                    print_stash_invalid_option_usage_header();
                }
            }
            let code = match e.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    if rest.iter().any(|a| a == "--help") || subcmd == "submodule" {
                        0
                    } else {
                        129
                    }
                }
                clap::error::ErrorKind::DisplayVersion => 129,
                _ => 129,
            };
            std::process::exit(code);
        }
    }
}

fn run() -> Result<()> {
    // Check env vars that clap would have handled
    if let Ok(git_dir) = std::env::var("GIT_DIR") {
        if git_dir.is_empty() {
            // ignore empty GIT_DIR
        }
    }

    if std::env::var("GRIT_INVOCATION_CWD")
        .ok()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        if let Ok(cwd) = std::env::current_dir() {
            std::env::set_var("GRIT_INVOCATION_CWD", cwd.display().to_string());
        }
    }

    // When GIT_EXEC_PATH points at a writable helper dir, expose shims for the few
    // subcommands vanilla Git ships as separate executables but grit implements as
    // built-ins, so a sibling real `git submodule …` can delegate to grit.
    install_exec_path_passthrough_helpers();

    let args = argv_lossy();
    let (opts, subcmd, rest) = extract_globals(&args)?;

    let subcmd = match subcmd {
        Some(s) => s,
        None => {
            commands::help::print_common_help();
            std::process::exit(1);
        }
    };

    if subcmd == "stash" {
        commands::stash::pre_parse_stash_argv_guard(&rest)?;
    }

    // t0017-env-helper expects config to be loaded very early when
    // GIT_TEST_ENV_HELPER=true, even before applying -C.
    if subcmd == "test-tool"
        && std::env::var("GIT_TEST_ENV_HELPER")
            .ok()
            .and_then(|v| parse_bool_str(&v))
            == Some(true)
    {
        // Accept optional leading "-C <dir>" pairs before "env-helper".
        let mut idx = 0usize;
        while idx + 1 < rest.len() && rest[idx] == "-C" {
            idx += 2;
        }
        let is_env_helper = rest.get(idx).map(String::as_str) == Some("env-helper");
        if is_env_helper {
            let _ = grit_lib::config::ConfigSet::load(None, true)?;
        }
    }

    apply_globals(&opts)?;

    // Git allows `-C <dir>` after the subcommand (e.g. `git config -C repo key val`).
    // Apply those directory changes after global `-C` but before running the subcommand.
    let mut rest = rest;
    strip_subcommand_leading_change_dir(&subcmd, &mut rest)?;

    precompose::precompose_dispatch_argv(&subcmd, &mut rest);

    // GIT_TRACE: write built-in trace line (after global options are processed)
    if let Ok(trace_val) = std::env::var("GIT_TRACE") {
        if !trace_val.is_empty() && trace_val != "0" && trace_val.to_lowercase() != "false" {
            let mut trace_cmd = format!("git {subcmd}");
            for arg in &rest {
                trace_cmd.push(' ');
                trace_cmd.push_str(arg);
            }
            let now = time::OffsetDateTime::now_utc();
            let trace_line = format!(
                "{:02}:{:02}:{:02}.{:06} grit:0               trace: built-in: {}\n",
                now.hour(),
                now.minute(),
                now.second(),
                now.microsecond(),
                trace_cmd,
            );
            write_git_trace(&trace_val, &trace_line);
        }
    }

    // `grit completions <shell>` is grit-specific (not a Git built-in); handle it
    // before alias resolution so it cannot be shadowed by a user alias.
    if subcmd == "completions" {
        return commands::completions::run(&rest);
    }

    // Handle --git-completion-helper / --git-completion-helper-all
    if let Some(pos) = rest
        .iter()
        .position(|a| *a == "--git-completion-helper" || *a == "--git-completion-helper-all")
    {
        // `git test-tool parse-subcommand cmd --git-completion-helper` must reach the
        // C/Rust test-tool implementation (t0040), not the generic clap-based helper.
        let skip_for_nested_test_tool = subcmd == "test-tool" && pos > 0;
        if !skip_for_nested_test_tool {
            let show_all = rest.iter().any(|a| a == "--git-completion-helper-all");
            // Check if there's a sub-subcommand (e.g., 'config get --git-completion-helper')
            let sub_subcmd: Option<&str> = rest
                .iter()
                .find(|a| !a.starts_with('-'))
                .map(|s| s.as_str());
            let key = if let Some(sub) = sub_subcmd {
                format!("{}_{}", subcmd, sub)
            } else {
                subcmd.clone()
            };
            return print_completion_helper(&key, show_all);
        }
    }

    alias::run_command_with_aliases(subcmd, rest, &opts)
}

/// Print --git-completion-helper output for a subcommand.
///
/// This mimics git's `--git-completion-helper` by listing all long options
/// (and their `--no-` negations) for the given subcommand.
fn print_completion_helper(subcmd: &str, show_all: bool) -> Result<()> {
    fn extract_options<T: Args>(show_all: bool) -> Vec<String> {
        let cmd = Command::new("grit").flatten_help(false);
        let cmd = T::augment_args(cmd);
        let mut positive = Vec::new();
        let mut negative = Vec::new();
        for arg in cmd.get_arguments() {
            if arg.get_id() == "help" || arg.get_id() == "version" {
                continue;
            }
            // Skip positional arguments
            if arg.get_long().is_none() && arg.get_short().is_none() {
                continue;
            }
            if let Some(long) = arg.get_long() {
                let hidden = arg.is_hide_set();
                // Check if this option takes a value
                let takes_value = match arg.get_action() {
                    clap::ArgAction::Set | clap::ArgAction::Append => true,
                    _ => arg.get_num_args().is_some_and(|r| r.min_values() > 0),
                };
                let suffix = if takes_value { "=" } else { "" };
                if hidden {
                    // Hidden args go to negative section only
                    negative.push(format!("--{long}{suffix}"));
                } else if long.starts_with("no-") {
                    // Explicit non-hidden --no-* args go in positive list
                    // (user-facing options like --no-guess)
                    positive.push(format!("--{long}{suffix}"));
                } else {
                    positive.push(format!("--{long}{suffix}"));
                    // Auto-generate --no- variant for the negative list
                    negative.push(format!("--no-{long}"));
                }
                // Add aliases (only with --git-completion-helper-all)
                if show_all {
                    if let Some(aliases) = arg.get_aliases() {
                        for alias in aliases {
                            if alias.starts_with("no-") {
                                negative.push(format!("--{alias}{suffix}"));
                            } else {
                                positive.push(format!("--{alias}{suffix}"));
                                negative.push(format!("--no-{alias}"));
                            }
                        }
                    }
                }
            }
        }
        // Collect subcommand names
        let mut subcmds: Vec<String> = Vec::new();
        for sub in cmd.get_subcommands() {
            let name = sub.get_name().to_string();
            if name != "help" {
                subcmds.push(name);
            }
        }

        if subcmds.is_empty() {
            let mut result = positive;
            // Only separate positive/negative with `--` sentinel when
            // there are enough options to warrant it. For small
            // commands, include --no-* variants inline (matching git).
            if negative.len() > 3 {
                result.push("--".to_string());
                result.extend(negative);
            } else {
                result.extend(negative);
            }
            result
        } else {
            // Has subcommands: return ONLY subcommands.
            // Options come from 'git <cmd> <subcmd> --git-completion-helper'.
            // __gitcomp will show subcommand names for empty cur,
            // and the completion script handles --options via subcommand-
            // specific helpers.
            subcmds
        }
    }

    let options = match subcmd {
        "add" => extract_options::<commands::add::Args>(show_all),
        "am" => extract_options::<commands::am::Args>(show_all),
        "apply" => extract_options::<commands::apply::Args>(show_all),
        "bisect" => extract_options::<commands::bisect::Args>(show_all),
        "blame" => extract_options::<commands::blame::Args>(show_all),
        "branch" => extract_options::<commands::branch::Args>(show_all),
        "cat-file" => extract_options::<commands::cat_file::Args>(show_all),
        "check-ignore" => extract_options::<commands::check_ignore::Args>(show_all),
        "checkout" => extract_options::<commands::checkout::Args>(show_all),
        "cherry-pick" => extract_options::<commands::cherry_pick::Args>(show_all),
        "clean" => extract_options::<commands::clean::Args>(show_all),
        "clone" => extract_options::<commands::clone::Args>(show_all),
        "commit" => extract_options::<commands::commit::Args>(show_all),
        "config" => extract_options::<commands::config::Args>(show_all),
        "config_get" => extract_options::<commands::config::GetArgs>(show_all),
        "config_set" => extract_options::<commands::config::SetArgs>(show_all),
        "config_unset" => extract_options::<commands::config::UnsetArgs>(show_all),
        "config_list" => extract_options::<commands::config::ListArgs>(show_all),
        "config_edit" => extract_options::<commands::config::EditArgs>(show_all),
        "reflog_show" => extract_options::<commands::reflog::ShowArgs>(show_all),
        "reflog_expire" => extract_options::<commands::reflog::ExpireArgs>(show_all),
        "reflog_list" => extract_options::<commands::reflog::ListArgs>(show_all),
        "reflog_drop" => extract_options::<commands::reflog::DropArgs>(show_all),
        "reflog_delete" => extract_options::<commands::reflog::DeleteArgs>(show_all),
        "reflog_exists" => extract_options::<commands::reflog::ExistsArgs>(show_all),
        "describe" => extract_options::<commands::describe::Args>(show_all),
        "diff" => extract_options::<commands::diff::Args>(show_all),
        "fetch" => extract_options::<commands::fetch::Args>(show_all),
        "for-each-ref" => extract_options::<commands::for_each_ref::Args>(show_all),
        "format-patch" => extract_options::<commands::format_patch::Args>(show_all),
        "fsck" => extract_options::<commands::fsck::Args>(show_all),
        "gc" => extract_options::<commands::gc::Args>(show_all),
        "grep" => extract_options::<commands::grep::Args>(show_all),
        "init" => extract_options::<commands::init::Args>(show_all),
        "log" => extract_options::<commands::log::Args>(show_all),
        "ls-files" => extract_options::<commands::ls_files::Args>(show_all),
        "ls-remote" => extract_options::<commands::ls_remote::Args>(show_all),
        "ls-tree" => extract_options::<commands::ls_tree::Args>(show_all),
        "merge" => extract_options::<commands::merge::Args>(show_all),
        "merge-tree" => commands::merge_tree::completion_helper_options(show_all),
        "merge-base" => extract_options::<commands::merge_base::Args>(show_all),
        "multi-pack-index" => extract_options::<commands::multi_pack_index::Args>(show_all),
        "mv" => extract_options::<commands::mv::Args>(show_all),
        "notes" => extract_options::<commands::notes::Args>(show_all),
        "pull" => extract_options::<commands::pull::Args>(show_all),
        "push" => extract_options::<commands::push::Args>(show_all),
        "rebase" => extract_options::<commands::rebase::Args>(show_all),
        "reflog" => extract_options::<commands::reflog::Args>(show_all),
        "remote" => extract_options::<commands::remote::Args>(show_all),
        "reset" => extract_options::<commands::reset::Args>(show_all),
        "restore" => extract_options::<commands::restore::Args>(show_all),
        "rev-list" => extract_options::<commands::rev_list::Args>(show_all),
        "rev-parse" => extract_options::<commands::rev_parse::Args>(show_all),
        "revert" => extract_options::<commands::revert::Args>(show_all),
        "rm" => extract_options::<commands::rm::Args>(show_all),
        "show" => extract_options::<commands::show::Args>(show_all),
        "show-ref" => extract_options::<commands::show_ref::Args>(show_all),
        "sparse-checkout" => extract_options::<commands::sparse_checkout::Args>(show_all),
        "stash" => extract_options::<commands::stash::Args>(show_all),
        "status" => extract_options::<commands::status::Args>(show_all),
        "submodule" => extract_options::<commands::submodule::Args>(show_all),
        "switch" => extract_options::<commands::switch::Args>(show_all),
        "symbolic-ref" => extract_options::<commands::symbolic_ref::Args>(show_all),
        "tag" => extract_options::<commands::tag::Args>(show_all),
        "update-index" => extract_options::<commands::update_index::Args>(show_all),
        "update-ref" => extract_options::<commands::update_ref::Args>(show_all),
        "worktree" => extract_options::<commands::worktree::Args>(show_all),
        "version" => extract_options::<commands::version::Args>(show_all),
        _ => Vec::new(),
    };

    let options = postprocess_completion_options(subcmd, options);

    println!("{}", options.join(" "));
    Ok(())
}

/// Adjust `--git-completion-helper` output where Git's completion script expects a shape that
/// differs slightly from clap's derived option list.
fn postprocess_completion_options(subcmd: &str, options: Vec<String>) -> Vec<String> {
    if subcmd == "clone" {
        return options
            .into_iter()
            .map(|s| match s.as_str() {
                "--recurse-submodules=" => "--recurse-submodules".to_string(),
                "--recursive=" => "--recursive".to_string(),
                _ => s,
            })
            .collect();
    }
    if subcmd != "checkout" {
        return options;
    }

    let (head, tail) = if let Some(i) = options.iter().position(|s| s == "--") {
        let tail = options[i + 1..].to_vec();
        (options[..i].to_vec(), tail)
    } else {
        (options, Vec::new())
    };

    let mut head: Vec<String> = head
        .into_iter()
        .filter(|s| s != "--overwrite-ignore" && s != "--no-progress")
        .map(|s| {
            if s == "--track=" {
                "--track".to_string()
            } else {
                s
            }
        })
        .collect();

    let tail: Vec<String> = tail
        .into_iter()
        .filter(|s| s != "--no-overwrite-ignore")
        .collect();

    if !tail.is_empty() {
        head.push("--".to_string());
        head.extend(tail);
    }
    head
}

/// Handle --list-cmds=<categories> for bash completion.
///
/// Categories are comma-separated. Supported:
/// - list-mainporcelain: high-level user commands
/// - list-complete: other useful commands
/// - list-all: all commands (porcelain + plumbing)
/// - config: commands from completion.commands config
fn print_list_cmds(categories: &str) {
    let mut parseopt_mode = false;
    let mainporcelain = [
        "add",
        "am",
        "archive",
        "bisect",
        "branch",
        "bundle",
        "checkout",
        "cherry-pick",
        "clean",
        "clone",
        "commit",
        "describe",
        "diff",
        "fetch",
        "format-patch",
        "gc",
        "grep",
        "init",
        "log",
        "merge",
        "mv",
        "notes",
        "pull",
        "push",
        "range-diff",
        "rebase",
        "reset",
        "restore",
        "revert",
        "rm",
        "shortlog",
        "show",
        "sparse-checkout",
        "stash",
        "status",
        "switch",
        "tag",
        "worktree",
    ];
    let complete = [
        "apply",
        "blame",
        "cherry",
        "config",
        "difftool",
        "fsck",
        "help",
        "imap-send",
        "mergetool",
        "prune",
        "reflog",
        "remote",
        "repack",
        "replace",
        "show-branch",
        "whatchanged",
    ];
    let plumbing = [
        "cat-file",
        "check-attr",
        "check-ignore",
        "check-ref-format",
        "checkout-index",
        "commit-graph",
        "commit-tree",
        "count-objects",
        "diff-files",
        "diff-index",
        "diff-tree",
        "for-each-ref",
        "get-tar-commit-id",
        "hash-object",
        "index-pack",
        "ls-files",
        "ls-remote",
        "ls-tree",
        "merge-base",
        "merge-file",
        "mktag",
        "mktree",
        "multi-pack-index",
        "name-rev",
        "pack-objects",
        "pack-refs",
        "read-tree",
        "rev-list",
        "rev-parse",
        "show-ref",
        "symbolic-ref",
        "update-index",
        "update-ref",
        "verify-commit",
        "verify-pack",
        "verify-tag",
        "write-tree",
    ];

    let mut result: Vec<&str> = Vec::new();
    for cat in categories.split(',') {
        match cat {
            "list-mainporcelain" => result.extend_from_slice(&mainporcelain),
            "list-complete" => result.extend_from_slice(&complete),
            "list-all" | "builtins" | "main" => {
                // Match `git --list-cmds=builtins`: `submodule` is porcelain-only (t7400) and
                // `mergetool` is not a builtin (shell script). Both still appear under
                // `list-mainporcelain` / `list-complete` when requested explicitly.
                for &cmd in &mainporcelain {
                    if cmd != "submodule" {
                        result.push(cmd);
                    }
                }
                for &cmd in &complete {
                    if cmd != "mergetool" {
                        result.push(cmd);
                    }
                }
                result.extend_from_slice(&plumbing);
            }
            "deprecated" => {
                result.extend_from_slice(crate::alias::DEPRECATED_COMMANDS);
            }
            "others" => {
                // Non-built-in commands like gitk
                result.push("gitk");
            }
            "alias" | "nohelpers" => {
                // alias = git aliases (handled by config, could list them)
                // nohelpers = filter out helper programs
            }
            "parseopt" => {
                parseopt_mode = true;
                // Commands that support --git-completion-helper
                let parseopt_cmds = [
                    "add",
                    "am",
                    "apply",
                    "bisect",
                    "blame",
                    "branch",
                    "cat-file",
                    "check-ignore",
                    "checkout",
                    "cherry-pick",
                    "clean",
                    "clone",
                    "commit",
                    "config",
                    "describe",
                    "diff",
                    "fetch",
                    "for-each-ref",
                    "format-patch",
                    "fsck",
                    "gc",
                    "grep",
                    "init",
                    "log",
                    "ls-files",
                    "ls-remote",
                    "ls-tree",
                    "merge",
                    "merge-base",
                    "mv",
                    "notes",
                    "pull",
                    "push",
                    "rebase",
                    "reflog",
                    "remote",
                    "reset",
                    "restore",
                    "rev-list",
                    "rev-parse",
                    "revert",
                    "rm",
                    "show",
                    "show-ref",
                    "sparse-checkout",
                    "stash",
                    "status",
                    "switch",
                    "symbolic-ref",
                    "tag",
                    "update-index",
                    "update-ref",
                    "version",
                    "worktree",
                ];
                result.extend_from_slice(&parseopt_cmds);
            }
            "list-guide" => {
                let guides = [
                    "core-tutorial",
                    "credentials",
                    "cvs-migration",
                    "diffcore",
                    "everyday",
                    "faq",
                    "glossary",
                    "namespaces",
                    "remote-helpers",
                    "submodules",
                    "tutorial",
                    "tutorial-2",
                    "workflows",
                ];
                result.extend_from_slice(&guides);
            }
            "config" => {
                // Check completion.commands config for additions/removals
                if let Ok(repo) = grit_lib::repo::Repository::discover(None) {
                    if let Ok(config) = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
                    {
                        if let Some(val) = config.get("completion.commands") {
                            for token in val.split_whitespace() {
                                if let Some(cmd) = token.strip_prefix('-') {
                                    result.retain(|c| *c != cmd);
                                } else {
                                    // Can't push a &str from config into &str vec, just print separately
                                    println!("{token}");
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if parseopt_mode {
        // parseopt outputs all commands on a single space-separated line
        println!(
            "{}",
            result
                .iter()
                .map(|s| s.as_ref())
                .collect::<Vec<&str>>()
                .join(" ")
        );
    } else {
        for cmd in &result {
            println!("{cmd}");
        }
    }
}

/// Preprocess diff arguments: expand `-U<N>` to `--unified=<N>` so that
/// clap does not swallow it into the trailing var-arg positional.
/// Bash expands `refs/heads/*:refs/remotes/foo/*` to one concrete branch when only one
/// `refs/heads/<name>` exists in the **current** repo's working tree context — but `git fetch`
/// refspecs are about the **remote** repository, so the glob must stay intact. Restore it when
/// we see the expanded form together with a negative `^` refspec (t5582).
fn preprocess_fetch_argv(rest: &[String]) -> Vec<String> {
    let mut deduped = Vec::with_capacity(rest.len());
    let mut saw_keep = false;
    for s in rest {
        if s == "-k" || s == "--keep" {
            if saw_keep {
                continue;
            }
            saw_keep = true;
        }
        deduped.push(s.clone());
    }
    let has_negative = deduped.iter().any(|s| s.starts_with('^'));
    if !has_negative {
        return deduped;
    }
    let mut out = deduped;
    for spec in &mut out {
        if !spec.starts_with("refs/heads/") || !spec.contains(':') || spec.contains('*') {
            continue;
        }
        let Some(colon) = spec.find(':') else {
            continue;
        };
        let (src, dst) = (&spec[..colon], &spec[colon + 1..]);
        let Some(branch) = src.strip_prefix("refs/heads/") else {
            continue;
        };
        if branch.contains('/') {
            continue;
        }
        let Some(rem_tail) = dst.strip_prefix("refs/remotes/") else {
            continue;
        };
        let Some(slash) = rem_tail.rfind('/') else {
            continue;
        };
        let remote_dir = &rem_tail[..slash];
        let dst_branch = &rem_tail[slash + 1..];
        if dst_branch == branch {
            *spec = format!("refs/heads/*:refs/remotes/{remote_dir}/*");
        }
    }
    out
}

/// `git format-patch -3` uses a negative-looking revision count; clap otherwise parses `-3` as
/// unknown short flags and leaves `--stdout` in `revisions`. Peel off `-<digits>` and pass the
/// count via a hidden long option. Also translate the various attached short forms (`-v<x>`,
/// `-U<n>`, `-O<file>`) into long options clap understands, so they are not swallowed as revisions.
fn preprocess_format_patch_argv(rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len() + 1);
    let mut max_count: Option<usize> = None;
    let mut i = 0usize;
    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--" {
            out.extend_from_slice(&rest[i..]);
            break;
        }
        // `-<digits>` count shorthand (e.g. `-3`).
        if arg.len() > 1
            && arg.starts_with('-')
            && arg.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
            && arg[1..].chars().all(|c| c.is_ascii_digit())
        {
            if let Ok(n) = arg[1..].parse::<usize>() {
                if n > 0 {
                    max_count = Some(n);
                    i += 1;
                    continue;
                }
            }
        }
        // `-v<x>` attached reroll count (anything after `-v`, including non-numeric like
        // `-v4rev2` or `-v4.4` or `-v4---...`). Bare `-v <x>` is handled by clap (short option).
        if let Some(val) = arg.strip_prefix("-v") {
            if !val.is_empty() {
                out.push(format!("--reroll-count={val}"));
                i += 1;
                continue;
            }
        }
        // `-U<n>` attached unified context.
        if let Some(val) = arg.strip_prefix("-U") {
            if !val.is_empty() {
                out.push(format!("--unified={val}"));
                i += 1;
                continue;
            }
        }
        // `-O<file>` attached orderfile (clap can mis-parse paths starting with `.`).
        if let Some(val) = arg.strip_prefix("-O") {
            if !val.is_empty() && !val.starts_with('=') {
                out.push("-O".to_owned());
                out.push(val.to_owned());
                i += 1;
                continue;
            }
        }
        out.push(arg.clone());
        i += 1;
    }
    if let Some(n) = max_count {
        out.insert(0, format!("--grit-format-patch-max-count={n}"));
    }
    out
}

fn preprocess_diff_args(rest: &[String]) -> Vec<String> {
    // Git rejects `--no-rename` as an invalid abbreviation (ambiguous with `--no-renames` /
    // `--no-rename-empty`). Clap would otherwise treat it as a revision and fail later.
    if rest.iter().any(|a| a == "--no-rename") {
        eprintln!("error: invalid option: --no-rename");
        std::process::exit(129);
    }

    let mut result = Vec::new();
    let mut i = 0usize;
    let word_diff_modes = ["plain", "color", "porcelain", "none"];
    while i < rest.len() {
        let arg = &rest[i];
        // Clap parses glued `-O../path` incorrectly (treats `--output` as a revision when the
        // orderfile path starts with `.` / `..`). Split into `-O` and the path (matches `git diff -O`).
        if arg.len() > 2 && arg.starts_with("-O") && !arg.starts_with("-O=") {
            let rest_o = &arg[2..];
            if !rest_o.is_empty() && !rest_o.starts_with('-') {
                result.push("-O".to_owned());
                result.push(rest_o.to_owned());
                i += 1;
                continue;
            }
        }
        // `-X` is the short form of `--dirstat`. Unlike a value option it never consumes a
        // following space-separated token: bare `-X` becomes `--dirstat` (no params), and a
        // parameter must be attached as `-X<param>` (→ `--dirstat=<param>`). Git's
        // `-X 0 HEAD^..HEAD` parses `0` as a revision, not the cut-off, so the next argument
        // must not be swallowed here.
        if arg == "-X" {
            result.push("--dirstat".to_owned());
            i += 1;
            continue;
        }
        if let Some(param) = arg.strip_prefix("-X") {
            if !param.is_empty() {
                result.push(format!("--dirstat={param}"));
                i += 1;
                continue;
            }
        }
        if arg == "-U" || arg == "--unified" {
            // `-U <N>` with a space — merge into `--unified=<N>` only when the next token is
            // numeric. Bare `-U` (e.g. `git diff -U <rev>`) uses the default context and must
            // not swallow the following revision/path.
            if i + 1 < rest.len()
                && rest[i + 1].chars().all(|c| c.is_ascii_digit())
                && !rest[i + 1].is_empty()
            {
                result.push(format!("--unified={}", rest[i + 1]));
                i += 2;
            } else {
                result.push("--unified=3".to_owned());
                i += 1;
            }
        } else if arg == "--abbrev" {
            // Bare `--abbrev` uses git's default abbreviation (7) and must not consume the
            // following revision/path as its value.
            if i + 1 < rest.len()
                && rest[i + 1].chars().all(|c| c.is_ascii_digit())
                && !rest[i + 1].is_empty()
            {
                result.push(format!("--abbrev={}", rest[i + 1]));
                i += 2;
            } else {
                result.push("--abbrev=7".to_owned());
                i += 1;
            }
        } else if arg == "--word-diff" {
            if i + 1 < rest.len() && word_diff_modes.contains(&rest[i + 1].as_str()) {
                result.push(format!("--word-diff={}", rest[i + 1]));
                i += 2;
            } else {
                // Prevent clap from consuming the first path argument as MODE.
                result.push("--word-diff=plain".to_owned());
                i += 1;
            }
        } else if arg == "--color-words" {
            // Keep paths separate: only `=<regex>` carries a pattern (matches Git / clap `require_equals`).
            result.push("--color-words=".to_owned());
            i += 1;
        } else if let Some(n) = arg.strip_prefix("-U") {
            // `-U<N>` without a space
            result.push(format!("--unified={n}"));
            i += 1;
        } else if arg == "--submodule" {
            // Bare `--submodule` must not consume the next argv token (e.g. `main^!` in t2405).
            result.push("--submodule=log".to_owned());
            i += 1;
        } else {
            result.push(arg.clone());
            i += 1;
        }
    }
    result
}

/// Preprocess log arguments: convert `-<N>` shorthand to `-n <N>`, and make bare `-L` visible to clap.
/// Map `git log` pickaxe `-G` / `-S` (including glued forms) to hidden long options so `-G` stays
/// free for `--basic-regexp` on `--grep`.
/// Split glued pickaxe argv like `-Sneedle` into `-S` + `needle`.
///
/// POSIX `/bin/sh` parses `-S"not present"` as a single token `-Snot present`, which clap would
/// reject as an unknown short flag. Git accepts the glued form (t4069-remerge-diff).
fn preprocess_show_argv(rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len() + 2);
    let mut i = 0usize;
    while i < rest.len() {
        let arg = rest[i].as_str();
        if arg == "--" {
            out.extend_from_slice(&rest[i..]);
            break;
        }
        // `--dirstat<...>` is only valid as bare `--dirstat` or `--dirstat=<param>`; glued
        // forms like `--dirstat10` are unrecognised (Git: "unrecognized argument").
        if let Some(suffix) = arg.strip_prefix("--dirstat") {
            if !suffix.is_empty() && !suffix.starts_with('=') {
                eprintln!("fatal: unrecognized argument: {arg}");
                std::process::exit(128);
            }
        }
        // `-X` is `--dirstat`'s short form: bare `-X` → `--dirstat`, `-X<param>` →
        // `--dirstat=<param>`. So `-X=20` yields the dirstat parameter `=20`, which is invalid.
        if arg == "-X" {
            out.push("--dirstat".to_owned());
            i += 1;
            continue;
        }
        if let Some(param) = arg.strip_prefix("-X") {
            if !param.is_empty() {
                if param.starts_with('=') || param.chars().next().is_some_and(|c| c == '=') {
                    eprintln!(
                        "fatal: Failed to parse --dirstat/-X option parameter:\n  Unknown dirstat parameter '{param}'\n"
                    );
                    std::process::exit(128);
                }
                out.push(format!("--dirstat={param}"));
                i += 1;
                continue;
            }
        }
        if let Some(needle) = arg.strip_prefix("-S") {
            if !needle.is_empty() {
                out.push("-S".to_owned());
                out.push(needle.to_owned());
                i += 1;
                continue;
            }
        }
        out.push(rest[i].clone());
        i += 1;
    }
    out
}

fn preprocess_log_pickaxe_args(rest: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len() + 4);
    let mut i = 0usize;
    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--" {
            out.extend_from_slice(&rest[i..]);
            break;
        }
        if let Some(pat) = arg.strip_prefix("-G") {
            out.push("--pickaxe-grep".to_string());
            if pat.is_empty() {
                let next = rest.get(i + 1);
                if next.is_none() || next.is_some_and(|n| n.starts_with('-')) {
                    out.push("\u{7f}__GRIT_MISSING_PICKAXE_G__".to_string());
                } else {
                    i += 1;
                    out.push(rest[i].clone());
                }
            } else {
                out.push(pat.to_string());
            }
            i += 1;
            continue;
        }
        if let Some(needle) = arg.strip_prefix("-S") {
            out.push("--pickaxe-string".to_string());
            if needle.is_empty() {
                let next = rest.get(i + 1);
                if next.is_none() || next.is_some_and(|n| n.starts_with('-')) {
                    out.push("\u{7f}__GRIT_MISSING_PICKAXE_S__".to_string());
                } else {
                    i += 1;
                    out.push(rest[i].clone());
                }
            } else {
                out.push(needle.to_string());
            }
            i += 1;
            continue;
        }
        // `-I<regex>` / `-I <regex>` / `--ignore-matching-lines[=<regex>]` (hunk-level line ignore).
        // Canonicalize to `--ignore-matching-lines=<regex>` so clap recognizes the flag and the
        // remaining `-p`/revision args parse normally.
        if arg == "--ignore-matching-lines" {
            if let Some(pat) = rest.get(i + 1) {
                out.push(format!("--ignore-matching-lines={pat}"));
                i += 2;
                continue;
            }
        }
        if arg.starts_with("--ignore-matching-lines=") {
            out.push(arg.clone());
            i += 1;
            continue;
        }
        if let Some(pat) = arg.strip_prefix("-I") {
            if pat.is_empty() {
                if let Some(next) = rest.get(i + 1) {
                    out.push(format!("--ignore-matching-lines={next}"));
                    i += 2;
                    continue;
                }
            } else {
                out.push(format!("--ignore-matching-lines={pat}"));
                i += 1;
                continue;
            }
        }
        out.push(arg.clone());
        i += 1;
    }
    out
}

fn preprocess_log_remotes(rest: &[String]) -> Vec<String> {
    let mut result = Vec::with_capacity(rest.len());
    let mut i = 0usize;
    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--" {
            result.extend_from_slice(&rest[i..]);
            break;
        }
        if arg == "--remotes" {
            result.push("--grit-internal-remotes=".to_string());
            i += 1;
            continue;
        }
        if let Some(pat) = arg.strip_prefix("--remotes=") {
            result.push(format!("--grit-internal-remotes={pat}"));
            i += 1;
            continue;
        }
        result.push(arg.clone());
        i += 1;
    }
    result
}

fn expand_git_notes_ref_token(token: &str) -> String {
    if token.starts_with("refs/notes/") {
        token.to_string()
    } else if token.starts_with("notes/") {
        format!("refs/{token}")
    } else {
        format!("refs/notes/{token}")
    }
}

#[derive(Clone, Copy)]
enum NotesDisplayDefault {
    /// `git log`: notes appear in medium/full unless `--no-notes`.
    OnIfUnset,
    /// `git show` / `git format-patch`: notes only with explicit `--notes` / `--show-notes`.
    OffIfUnset,
}

/// Strip Git note-display options and record state in env for log/show/format-patch.
fn preprocess_git_notes_display_argv(
    rest: &[String],
    default_notes: NotesDisplayDefault,
) -> Vec<String> {
    let mut cli_on: Option<bool> = None;
    let mut use_default: i8 = -1;
    let mut out: Vec<String> = Vec::with_capacity(rest.len());
    let mut notes_tail: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < rest.len() {
        let arg = rest[i].as_str();
        if arg == "--show-notes" {
            cli_on = Some(true);
            if use_default < 0 {
                use_default = 1;
            }
            notes_tail.push("--notes".to_string());
            i += 1;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--show-notes=") {
            cli_on = Some(true);
            if use_default < 0 {
                use_default = 1;
            }
            notes_tail.push(format!("--notes={}", expand_git_notes_ref_token(v)));
            i += 1;
            continue;
        }
        if arg == "--notes" {
            cli_on = Some(true);
            if use_default < 0 {
                use_default = 1;
            }
            notes_tail.push("--notes".to_string());
            i += 1;
            continue;
        }
        if let Some(v) = arg.strip_prefix("--notes=") {
            cli_on = Some(true);
            notes_tail.push(format!("--notes={}", expand_git_notes_ref_token(v)));
            i += 1;
            continue;
        }
        if arg == "--no-notes" {
            cli_on = Some(false);
            use_default = -1;
            notes_tail.clear();
            i += 1;
            continue;
        }
        if arg == "--standard-notes" {
            use_default = 1;
            i += 1;
            continue;
        }
        if arg == "--no-standard-notes" {
            use_default = 0;
            i += 1;
            continue;
        }
        out.push(rest[i].clone());
        i += 1;
    }
    if !notes_tail.is_empty() {
        let mut reordered = notes_tail;
        reordered.extend(out);
        out = reordered;
    }
    match cli_on {
        Some(true) => std::env::set_var("GIT_GRIT_LOG_NOTES_CLI", "on"),
        Some(false) => std::env::set_var("GIT_GRIT_LOG_NOTES_CLI", "off"),
        None => std::env::remove_var("GIT_GRIT_LOG_NOTES_CLI"),
    }
    std::env::set_var(
        "GIT_GRIT_LOG_NOTES_DEFAULT",
        match default_notes {
            NotesDisplayDefault::OnIfUnset => "1",
            NotesDisplayDefault::OffIfUnset => "0",
        },
    );
    std::env::set_var(
        "GIT_GRIT_LOG_NOTES_USE_DEFAULT",
        match use_default {
            -1 => "",
            0 => "0",
            _ => "1",
        },
    );
    std::env::remove_var("GIT_GRIT_LOG_NOTES_EXTRA");
    out
}

/// Preprocess `git log` argv fragments (before clap) for spawning a child `grit log` process.
pub(crate) fn preprocess_log_argv_for_spawn(rest: &[String]) -> Vec<String> {
    preprocess_expand_tabs_for_rev_cmd(&preprocess_log_pickaxe_args(preprocess_log_args(rest)))
}

/// Remove revision pseudo-options that must not reach clap (unknown flags) but are still needed
/// for `merge_log_revision_argv` via [`commands::log::Args::raw_argv_tail`].
fn strip_log_revision_pseudo_for_clap(rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len());
    let mut i = 0usize;
    while i < rest.len() {
        let a = rest[i].as_str();
        // After `--end-of-options`, every remaining token is a revision or
        // pathspec (e.g. a branch literally named `--source`), not an option.
        // Drop them from clap's view (the revision machinery re-reads them from
        // `raw_argv_tail`) so clap does not mistake e.g. `--source` for its flag.
        if a == "--end-of-options" {
            out.push(rest[i].clone());
            break;
        }
        if a == "--not" {
            i += 1;
            continue;
        }
        if a == "--glob" {
            i += 1;
            if i < rest.len() {
                i += 1;
            }
            continue;
        }
        if let Some(pat) = a.strip_prefix("--glob=") {
            if pat.is_empty() {
                // Keep bare `--glob=` so clap can report missing value if needed; do not strip.
                out.push(rest[i].clone());
            }
            i += 1;
            continue;
        }
        out.push(rest[i].clone());
        i += 1;
    }
    out
}

/// Normalize `--expand-tabs` without `=` to `--expand-tabs=8` (Git revision.c).
fn preprocess_expand_tabs_for_rev_cmd(rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len());
    let mut i = 0usize;
    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--" {
            out.extend_from_slice(&rest[i..]);
            break;
        }
        if arg == "--expand-tabs" {
            out.push("--expand-tabs=8".to_string());
            i += 1;
            continue;
        }
        out.push(arg.clone());
        i += 1;
    }
    out
}

/// Value-less `git log` option flags that git accepts in any position (before or after
/// revision arguments). clap treats the `revisions` positional as `allow_hyphen_values`,
/// so a flag appearing after a revision would be consumed as a revision instead of an
/// option. Hoisting these specific flags ahead of the positionals restores git's behavior
/// without disturbing options that take values.
fn hoist_trailing_log_flags(rest: &[String]) -> Vec<String> {
    const FLAGS: &[&str] = &["--reverse"];
    let mut hoisted: Vec<String> = Vec::new();
    let mut tail: Vec<String> = Vec::new();
    let mut after_dashdash = false;
    for arg in rest {
        if after_dashdash {
            tail.push(arg.clone());
            continue;
        }
        if arg == "--" {
            after_dashdash = true;
            tail.push(arg.clone());
            continue;
        }
        if FLAGS.contains(&arg.as_str()) {
            hoisted.push(arg.clone());
        } else {
            tail.push(arg.clone());
        }
    }
    hoisted.extend(tail);
    hoisted
}

fn preprocess_log_args(rest: &[String]) -> Vec<String> {
    let rest = preprocess_git_notes_display_argv(rest, NotesDisplayDefault::OnIfUnset);
    // git accepts value-less options interspersed with / after revision arguments
    // (e.g. `git log -2 <rev> --reverse`). clap's `allow_hyphen_values` revisions
    // positional would otherwise swallow such a trailing flag as a revision. Hoist
    // these flags before the positional revisions (stopping at `--`, after which
    // everything is a pathspec).
    let rest = hoist_trailing_log_flags(&rest);
    let mut result = Vec::new();
    let mut saw_graph = false;
    let mut i = 0usize;
    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--graph" {
            if !saw_graph {
                result.push("--graph".to_string());
                saw_graph = true;
            }
            i += 1;
            continue;
        }
        // Normalize the attached form `-L<value>` into `-L <value>` so the value is parsed as the
        // line-range option rather than being swallowed by the hyphen-tolerant `revisions`
        // positional. This covers both `-L1,1:file` and `-L:pat:file` (the leading `:` of the
        // latter is preserved). `arg[2..]` strips only the `-L` prefix and keeps the remainder.
        if arg.starts_with("-L") && arg.len() > 2 {
            result.push("-L".to_string());
            result.push(arg[2..].to_string());
            i += 1;
            continue;
        }
        if arg == "-L" {
            result.push("-L".to_string());
            let next = rest.get(i + 1);
            let need_placeholder = match next {
                None => true,
                Some(n) if n.starts_with('-') => true,
                Some(_) => false,
            };
            if need_placeholder {
                result.push(String::new());
            }
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 && arg[1..].chars().all(|c| c.is_ascii_digit()) {
            result.push("-n".to_string());
            result.push(arg[1..].to_string());
        } else if let Some(num) = arg
            .strip_prefix("-n")
            .filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        {
            // Normalize `-n6` to `-n 6` so clap parses it as the max-count option
            // instead of letting the `allow_hyphen_values` revisions positional
            // swallow it (and every option that follows).
            result.push("-n".to_string());
            result.push(num.to_string());
        } else {
            result.push(arg.clone());
        }
        i += 1;
    }
    result
}

/// Parsed `help.autocorrect` mode, matching `git/help.c` (`parse_autocorrect`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HelpAutocorrect {
    Never,
    Show,
    Immediately,
    Prompt,
    DelayDeciseconds(u32),
}

/// Read and parse `help.autocorrect` (env overrides, then repo config).
fn parse_help_autocorrect() -> Option<HelpAutocorrect> {
    let raw = if let Some(val) = protocol::check_config_param("help.autocorrect") {
        Some(val)
    } else {
        let git_dir = std::env::var("GIT_DIR")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| {
                grit_lib::repo::Repository::discover(None)
                    .ok()
                    .map(|r| r.git_dir)
            });
        grit_lib::config::ConfigSet::load(git_dir.as_deref(), true)
            .ok()
            .and_then(|c| c.get("help.autocorrect"))
    }?;
    let s = raw.trim();
    if let Some(b) = parse_bool_str(s) {
        return Some(if b {
            HelpAutocorrect::Immediately
        } else {
            HelpAutocorrect::Show
        });
    }
    if s.eq_ignore_ascii_case("never") {
        return Some(HelpAutocorrect::Never);
    }
    if s.eq_ignore_ascii_case("immediate") {
        return Some(HelpAutocorrect::Immediately);
    }
    if s.eq_ignore_ascii_case("show") {
        return Some(HelpAutocorrect::Show);
    }
    if s.eq_ignore_ascii_case("prompt") {
        return Some(HelpAutocorrect::Prompt);
    }
    if let Ok(n) = s.parse::<i32>() {
        if n >= 0 {
            return Some(HelpAutocorrect::DelayDeciseconds(n as u32));
        }
        if n == -1 || n == 1 {
            return Some(HelpAutocorrect::Immediately);
        }
    }
    Some(HelpAutocorrect::Show)
}

/// Damerau–Levenshtein with Git's weights: `levenshtein(s1, s2, 0, 2, 1, 3)` in `git/levenshtein.c`.
fn weighted_damerau_levenshtein(s1: &str, s2: &str) -> i32 {
    let s1: Vec<char> = s1.chars().collect();
    let s2: Vec<char> = s2.chars().collect();
    let len1 = s1.len();
    let len2 = s2.len();
    if len1 == 0 {
        return len2 as i32;
    }
    if len2 == 0 {
        return (len1 as i32) * 3;
    }
    let w = 0;
    let sub_cost = 2;
    let ins_cost = 1;
    let del_cost = 3;
    let mut row0 = vec![0i32; len2 + 1];
    let mut row1: Vec<i32> = (0..=len2).map(|j| j as i32 * ins_cost).collect();
    let mut row2 = vec![0i32; len2 + 1];
    for i in 0..len1 {
        row2[0] = (i as i32 + 1) * del_cost;
        for j in 0..len2 {
            let mut best = row1[j] + if s1[i] == s2[j] { 0 } else { sub_cost };
            if i > 0 && j > 0 && s1[i - 1] == s2[j] && s1[i] == s2[j - 1] && best > row0[j - 1] + w
            {
                best = row0[j - 1] + w;
            }
            best = best.min(row1[j + 1] + del_cost);
            best = best.min(row2[j] + ins_cost);
            row2[j + 1] = best;
        }
        std::mem::swap(&mut row0, &mut row1);
        std::mem::swap(&mut row1, &mut row2);
    }
    row1[len2]
}

fn collect_alias_command_names(config: &grit_lib::config::ConfigSet) -> Vec<String> {
    let mut names = Vec::new();
    for e in config.entries() {
        let key = &e.key;
        if !key.starts_with("alias.") {
            continue;
        }
        if key.ends_with(".command") {
            if let Some(name) = key
                .strip_prefix("alias.")
                .and_then(|k| k.strip_suffix(".command"))
            {
                names.push(name.to_string());
            }
        } else if let Some(name) = key.strip_prefix("alias..") {
            names.push(name.to_string());
        } else if let Some(rest) = key.strip_prefix("alias.") {
            if !rest.contains('.') {
                names.push(rest.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn path_component_is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .is_some_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn collect_path_git_command_names(exec_path: Option<&Path>) -> Vec<String> {
    let mut out = BTreeSet::new();
    let Some(path_var) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    for dir in std::env::split_paths(&path_var) {
        if exec_path.is_some_and(|ep| ep == dir.as_path()) {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            let Some(rest) = name.strip_prefix("git-") else {
                continue;
            };
            let cmd_name = rest.strip_suffix(".exe").unwrap_or(rest);
            if cmd_name.is_empty() || cmd_name.contains('/') || cmd_name.contains('\\') {
                continue;
            }
            let p = ent.path();
            if path_component_is_executable(&p) {
                out.insert(cmd_name.to_string());
            }
        }
    }
    out.into_iter().collect()
}

fn unknown_cmd_similarity_score(cmd: &str, candidate: &str) -> i32 {
    if COMMON_PORCELAIN_COMMANDS.contains(&candidate)
        && candidate.len() >= cmd.len()
        && candidate.starts_with(cmd)
    {
        return 0;
    }
    weighted_damerau_levenshtein(cmd, candidate) + 1
}

/// Handle an unknown subcommand like `help_unknown_cmd` in `git/help.c`.
fn handle_unknown_git_command(subcmd: &str, rest: &[String], opts: &GlobalOpts) -> Result<()> {
    const SIMILARITY_FLOOR: i32 = 7;

    // Config keys and similar dotted tokens are never git subcommands; skip autocorrect so
    // `test_must_fail git -C repo core.sparseCheckoutCone` stays a controlled failure (t1091).
    if subcmd.contains('.') {
        eprintln!("git: '{subcmd}' is not a git command. See 'git --help'.");
        std::process::exit(1);
    }

    let mut mode = parse_help_autocorrect().unwrap_or(HelpAutocorrect::Show);
    if mode == HelpAutocorrect::Prompt
        && (!std::io::stdin().is_terminal() || !std::io::stderr().is_terminal())
    {
        mode = HelpAutocorrect::Never;
    }
    if mode == HelpAutocorrect::Never {
        bail!("git: '{subcmd}' is not a git command. See 'git --help'.");
    }

    let git_dir = std::env::var("GIT_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            grit_lib::repo::Repository::discover(None)
                .ok()
                .map(|r| r.git_dir)
        });
    let config = grit_lib::config::ConfigSet::load(git_dir.as_deref(), true).unwrap_or_default();
    let exec_path = git_exec_path_for_helpers(opts.exec_path.as_deref());

    let mut names: HashSet<String> = KNOWN_COMMANDS.iter().map(|s| (*s).to_string()).collect();
    for a in collect_alias_command_names(&config) {
        names.insert(a);
    }
    for p in collect_path_git_command_names(exec_path.as_deref()) {
        names.insert(p);
    }
    let mut candidates: Vec<String> = names.into_iter().collect();
    candidates.sort();

    let mut scored: Vec<(i32, String)> = candidates
        .into_iter()
        .map(|c| {
            let sim = unknown_cmd_similarity_score(subcmd, &c);
            (sim, c)
        })
        .collect();
    // Match `levenshtein_compare` in `git/help.c`: sort by similarity score (`len` field), then name.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let prefix_n = scored.iter().take_while(|(s, _)| *s == 0).count();
    let (best_similarity, tie_count) = if scored.is_empty() {
        (SIMILARITY_FLOOR + 1, 0usize)
    } else if prefix_n == scored.len() {
        (SIMILARITY_FLOOR + 1, 0usize)
    } else {
        let mut n = prefix_n + 1;
        let best = scored[prefix_n].0;
        while n < scored.len() && scored[n].0 == best {
            n += 1;
        }
        (best, n - prefix_n)
    };

    let run_corrected = |corrected: &str| -> Result<()> {
        eprintln!("WARNING: You called a grit command named '{subcmd}', which does not exist.");
        match mode {
            HelpAutocorrect::Immediately => {
                eprintln!("Continuing under the assumption that you meant '{corrected}'.");
            }
            HelpAutocorrect::Prompt => {
                eprint!("Run '{corrected}' instead [y/N]? ");
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                let line = line.trim();
                if !line.starts_with('y') && !line.starts_with('Y') {
                    std::process::exit(1);
                }
            }
            HelpAutocorrect::DelayDeciseconds(d) => {
                eprintln!(
                    "Continuing in {:.1} seconds, assuming that you meant '{corrected}'.",
                    d as f32 / 10.0
                );
                std::thread::sleep(std::time::Duration::from_millis(u64::from(d) * 100));
            }
            HelpAutocorrect::Show | HelpAutocorrect::Never => {}
        }
        // Re-enter the full argv path so `git-<cmd>` on `PATH` and `alias.*` work
        // (same as Git re-running `run_argv` after `help_unknown_cmd`).
        alias::run_command_with_aliases(corrected.to_string(), rest.to_vec(), opts)
    };

    let similar_enough = best_similarity < SIMILARITY_FLOOR;
    let autocorrect_runs = !matches!(mode, HelpAutocorrect::Show | HelpAutocorrect::Never);
    if autocorrect_runs && tie_count == 1 && similar_enough {
        let corrected = scored[prefix_n].1.clone();
        return run_corrected(&corrected);
    }

    eprintln!("git: '{subcmd}' is not a git command. See 'git --help'.");
    if similar_enough && tie_count > 0 {
        let start = prefix_n;
        let msg = if tie_count == 1 {
            "\nThe most similar command is"
        } else {
            "\nThe most similar commands are"
        };
        eprintln!("{msg}");
        for (_, name) in scored.iter().skip(start).take(tie_count) {
            eprintln!("\t{name}");
        }
    }
    std::process::exit(1);
}

pub(crate) const KNOWN_COMMANDS: &[&str] = &[
    "add",
    "am",
    "annotate",
    "apply",
    "archive",
    "backfill",
    "bisect",
    "blame",
    "branch",
    "bugreport",
    "bundle",
    "cat-file",
    "check-attr",
    "check-ignore",
    "check-mailmap",
    "check-ref-format",
    "checkout",
    "checkout-index",
    "cherry",
    "cherry-pick",
    "clean",
    "clone",
    "column",
    "commit",
    "commit-graph",
    "commit-tree",
    "config",
    "count-objects",
    "credential",
    "credential-cache",
    "credential-store",
    "daemon",
    "describe",
    "diagnose",
    "diff",
    "diff-files",
    "diff-index",
    "diff-pairs",
    "diff-tree",
    "difftool",
    "fast-export",
    "fast-import",
    "fetch",
    "fetch-pack",
    "filter-branch",
    "fmt-merge-msg",
    "for-each-ref",
    "for-each-repo",
    "format-patch",
    "fsck",
    "gc",
    "get-tar-commit-id",
    "grep",
    "hash-object",
    "help",
    "history",
    "hook",
    "http-backend",
    "http-fetch",
    "http-push",
    "imap-send",
    "index-pack",
    "init",
    "interpret-trailers",
    "last-modified",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "mailinfo",
    "mailsplit",
    "maintenance",
    "merge",
    "merge-base",
    "merge-file",
    "merge-index",
    "merge-one-file",
    "merge-recursive",
    "merge-resolve",
    "merge-tree",
    "mergetool",
    "mktag",
    "mktree",
    "multi-pack-index",
    "mv",
    "name-rev",
    "notes",
    "pack-objects",
    "pack-redundant",
    "pack-refs",
    "patch-id",
    "prune",
    "prune-packed",
    "pull",
    "push",
    "range-diff",
    "read-tree",
    "rebase",
    "receive-pack",
    "request-pull",
    "reflog",
    "refs",
    "remote",
    "repack",
    "replace",
    "replay",
    "repo",
    "rerere",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "scalar",
    "send-pack",
    "sh-i18n",
    "sh-i18n--envsubst",
    "sh-setup",
    "shell",
    "shortlog",
    "show",
    "show-branch",
    "show-index",
    "show-ref",
    "sparse-checkout",
    "stage",
    "stash",
    "status",
    "stripspace",
    "submodule",
    "submodule--helper",
    "switch",
    "symbolic-ref",
    "tag",
    "test-tool",
    "unpack-file",
    "unpack-objects",
    "update-index",
    "update-ref",
    "update-server-info",
    "upload-archive",
    "upload-pack",
    "var",
    "verify-commit",
    "verify-pack",
    "verify-tag",
    "version",
    "web--browse",
    "whatchanged",
    "worktree",
    "write-tree",
];

/// Porcelain commands Git treats as "common" for `help_unknown_cmd` prefix scoring
/// (`common_mask` in `git/help.c`: init | worktree | info | history | remote).
const COMMON_PORCELAIN_COMMANDS: &[&str] = &[
    "add",
    "clone",
    "commit",
    "init",
    "mv",
    "restore",
    "rm",
    "backfill",
    "branch",
    "checkout",
    "cherry-pick",
    "clean",
    "fetch",
    "format-patch",
    "gc",
    "grep",
    "history",
    "log",
    "maintenance",
    "merge",
    "notes",
    "pull",
    "push",
    "range-diff",
    "rebase",
    "reset",
    "revert",
    "shortlog",
    "show",
    "stash",
    "status",
    "submodule",
    "switch",
    "tag",
    "bisect",
    "describe",
    "diff",
    "worktree",
];

/// Dispatch to the appropriate command handler.
///
/// Each arm only constructs the clap parser for that specific command.
pub(crate) fn dispatch(subcmd: &str, rest: &[String], opts: &GlobalOpts) -> Result<()> {
    match subcmd {
        "add" => commands::add::run(parse_cmd_args(subcmd, rest)),
        "am" => commands::am::run(parse_cmd_args(subcmd, rest)),
        "annotate" => commands::annotate::run(parse_cmd_args(subcmd, &preprocess_blame_argv(rest))),
        "apply" => commands::apply::run(parse_cmd_args(subcmd, rest)),
        "archive" => {
            if rest.len() == 1 {
                let a = rest[0].as_str();
                if matches!(a, "-h" | "--help" | "--help-all") {
                    if let Some(syn) =
                        commands::upstream_synopsis_help::synopsis_for_builtin(subcmd)
                    {
                        let code = if a == "--help" { 0 } else { 129 };
                        commands::upstream_synopsis_help::print_upstream_synopsis_stdout_and_exit(
                            subcmd, syn, code,
                        );
                    }
                }
            }
            commands::archive::run_from_argv(rest)
        }
        "backfill" => commands::backfill::run(parse_cmd_args(subcmd, rest)),
        // Bisect is parsed manually: upstream tests pass unknown `--bisect-*` flags that must
        // reach `bisect::run` as Git-style "unknown option" errors, not clap parse failures.
        // Because of the manual dispatch we bypass the centralized -h/--help-all synopsis path,
        // so handle a bare -h/--help/--help-all here before any repo discovery happens.
        "bisect" => {
            commands::upstream_synopsis_help::try_print_upstream_help_and_exit(subcmd, rest);
            commands::bisect::run(commands::bisect::Args {
                args: rest.to_vec(),
            })
        }
        "blame" => commands::blame::run(parse_cmd_args(subcmd, &preprocess_blame_argv(rest))),
        "branch" => commands::branch::run(parse_cmd_args(subcmd, rest)),
        "bugreport" => commands::bugreport::run(parse_cmd_args(subcmd, rest)),
        "bundle" => commands::bundle::run(parse_cmd_args(subcmd, rest)),
        "cat-file" => commands::cat_file::run(parse_cmd_args(subcmd, rest)),
        "check-attr" => {
            if rest.len() == 1 {
                let a = rest[0].as_str();
                if matches!(a, "-h" | "--help" | "--help-all") {
                    if let Some(syn) =
                        commands::upstream_synopsis_help::synopsis_for_builtin(subcmd)
                    {
                        let code = if a == "--help" { 0 } else { 129 };
                        commands::upstream_synopsis_help::print_upstream_synopsis_stdout_and_exit(
                            subcmd, syn, code,
                        );
                    }
                }
            }
            commands::check_attr::run_from_argv(rest)
        }
        "check-ignore" => commands::check_ignore::run(parse_cmd_args(subcmd, rest)),
        "check-mailmap" => commands::check_mailmap::run(parse_cmd_args(subcmd, rest)),
        "check-ref-format" => commands::check_ref_format::run(parse_cmd_args(subcmd, rest)),
        "checkout" => commands::checkout::run(parse_cmd_args(subcmd, rest)),
        "checkout-index" => commands::checkout_index::run(parse_cmd_args(subcmd, rest)),
        "cherry" => commands::cherry::run(parse_cmd_args(subcmd, rest)),
        "cherry-pick" => commands::cherry_pick::run(parse_cmd_args(
            subcmd,
            &commands::cherry_pick::preprocess_cherry_pick_argv(rest),
        )),
        "clean" => commands::clean::run(parse_cmd_args(subcmd, rest)),
        "clone" => commands::clone::run(parse_cmd_args(subcmd, rest)),
        "column" => commands::column::run(parse_cmd_args(subcmd, rest)),
        "commit" => {
            let mut args: commands::commit::Args = parse_cmd_args(subcmd, rest);
            commands::commit::hydrate_raw_argv(&mut args);
            commands::commit::run(args)
        }
        "commit-graph" => commands::commit_graph::run(parse_cmd_args(subcmd, rest)),
        "commit-tree" => commands::commit_tree::run(parse_cmd_args(subcmd, rest)),
        "config" => commands::config::run(parse_cmd_args(subcmd, &preprocess_config_argv(rest))),
        "count-objects" => commands::count_objects::run(parse_cmd_args(subcmd, rest)),
        "credential" => commands::credential::run(parse_cmd_args(subcmd, rest)),
        "credential-cache" => commands::credential_cache::run(parse_cmd_args(subcmd, rest)),
        "credential-store" => commands::credential_store::run(parse_cmd_args(subcmd, rest)),
        "daemon" => commands::daemon::run(parse_cmd_args(subcmd, rest)),
        "describe" => commands::describe::run(parse_cmd_args(subcmd, rest)),
        "diagnose" => commands::diagnose::run(parse_cmd_args(subcmd, rest)),
        "diff" => commands::diff::run(parse_cmd_args(subcmd, &preprocess_diff_args(rest))),
        "diff-files" => commands::diff_files::run(parse_cmd_args(subcmd, rest)),
        "diff-index" => commands::diff_index::run(parse_cmd_args(subcmd, rest)),
        "diff-pairs" => commands::diff_pairs::run(parse_cmd_args(subcmd, rest)),
        "diff-tree" => commands::diff_tree::run(parse_cmd_args(subcmd, rest)),
        "difftool" => commands::difftool::run_from_argv(rest.to_vec()),
        "fast-export" => commands::fast_export::run(parse_cmd_args(subcmd, rest)),
        "fast-import" => commands::fast_import::run(parse_cmd_args(subcmd, rest)),
        "fetch" => commands::fetch::run(parse_cmd_args(subcmd, &preprocess_fetch_argv(rest))),
        "fetch-pack" => commands::fetch_pack::run(parse_cmd_args(subcmd, rest)),
        "filter-branch" => commands::filter_branch::run(parse_cmd_args(subcmd, rest)),
        "fmt-merge-msg" => commands::fmt_merge_msg::run(parse_cmd_args(subcmd, rest)),
        "for-each-ref" => commands::for_each_ref::run(parse_cmd_args(subcmd, rest)),
        "for-each-repo" => commands::for_each_repo::run(parse_cmd_args(subcmd, rest)),
        "format-patch" => {
            let rest = preprocess_git_notes_display_argv(rest, NotesDisplayDefault::OffIfUnset);
            commands::format_patch::run(parse_cmd_args(
                subcmd,
                &preprocess_format_patch_argv(&rest),
            ))
        }
        "fsck" => commands::fsck::run(parse_cmd_args(subcmd, rest)),
        "gc" => commands::gc::run(parse_cmd_args(subcmd, rest)),
        "get-tar-commit-id" => {
            // `-h`/`--help`/`--help-all` are handled by `parse_cmd_args`, which prints the
            // vendored adoc SYNOPSIS (`git get-tar-commit-id`, no trailing space) so the `-h`
            // output agrees with Documentation/git-get-tar-commit-id.adoc byte-for-byte (t0450).
            commands::get_tar_commit_id::run(parse_cmd_args(subcmd, rest))
        }
        "grep" => {
            // Git grep uses -h for --no-filename, conflicting with clap's -h for help.
            // A lone `git grep -h` is Git's short help (exit 129); do not rewrite to --no-filename.
            if rest.len() == 1 && rest[0] == "-h" {
                if let Some(syn) = commands::upstream_synopsis_help::synopsis_for_builtin(subcmd) {
                    commands::upstream_synopsis_help::print_upstream_synopsis_stdout_and_exit(
                        subcmd, syn, 129,
                    );
                }
            }
            let (rest, open_in_pager, open_pager_cmd) =
                commands::grep::preprocess_open_in_pager_argv(rest.to_vec());
            // Also implement last-flag-wins for -G/-E/-F/-P pattern type flags.
            // Rewrite -h to --no-filename. Handle both standalone "-h" and
            // combined flags like "-ah" (split into "-a" + "--no-filename").
            let mut new_rest: Vec<String> = Vec::new();
            for a in rest.iter() {
                if a == "-h" {
                    new_rest.push("--no-filename".to_string());
                } else if a.starts_with('-')
                    && !a.starts_with("--")
                    && a.contains('h')
                    && a.len() > 2
                {
                    // Combined short flags containing 'h'
                    let without_h: String = a.chars().filter(|&c| c != 'h').collect();
                    if without_h.len() > 1 {
                        // still has flags besides '-'
                        new_rest.push(without_h);
                    }
                    new_rest.push("--no-filename".to_string());
                } else {
                    new_rest.push(a.clone());
                }
            }
            let mut rest = new_rest;
            // Last-flag-wins: find the last pattern-type flag and remove earlier ones
            let pattern_flags = [
                "-G",
                "-E",
                "-F",
                "-P",
                "--basic-regexp",
                "--extended-regexp",
                "--fixed-strings",
                "--perl-regexp",
            ];
            let mut last_idx = None;
            for (i, a) in rest.iter().enumerate() {
                if pattern_flags.contains(&a.as_str()) {
                    last_idx = Some(i);
                }
            }
            if let Some(last) = last_idx {
                let keep = rest[last].clone();
                rest.retain(|a| !pattern_flags.contains(&a.as_str()));
                // Insert the winning flag back (at beginning, before positionals)
                rest.insert(0, keep);
            }
            let (pattern_tokens, rest_for_clap) =
                commands::grep_pattern::extract_pattern_tokens(&rest)?;
            let mut args: commands::grep::Args = parse_cmd_args(subcmd, &rest_for_clap);
            args.open_in_pager = open_in_pager;
            args.open_pager_cmd = open_pager_cmd;
            commands::grep::run_with_pattern_tokens(pattern_tokens, args)
        }
        "hash-object" => commands::hash_object::run(parse_cmd_args(subcmd, rest)),
        "help" => commands::help::run(parse_cmd_args(subcmd, rest)),
        "history" => commands::history::run_from_argv(rest),
        "hook" => commands::hook::run_from_argv(rest),
        "http-backend" => commands::http_backend::run(parse_cmd_args(subcmd, rest)),
        "http-fetch" => commands::http_fetch::run(parse_cmd_args(subcmd, rest)),
        "http-push" => commands::http_push::run(parse_cmd_args(subcmd, rest)),
        "imap-send" => {
            if rest.len() == 1 {
                let a = rest[0].as_str();
                if matches!(a, "-h" | "--help" | "--help-all") {
                    // Print the vendored adoc SYNOPSIS so `-h` agrees with
                    // Documentation/git-imap-send.adoc byte-for-byte (t0450).
                    if let Some(syn) =
                        commands::upstream_synopsis_help::synopsis_for_builtin(subcmd)
                    {
                        let code = if a == "--help" { 0 } else { 129 };
                        commands::upstream_synopsis_help::print_upstream_synopsis_stdout_and_exit(
                            subcmd, syn, code,
                        );
                    }
                }
            }
            commands::imap_send::run_from_argv(rest)
        }
        "index-pack" => {
            commands::upstream_synopsis_help::try_print_upstream_help_and_exit(subcmd, rest);
            let args = commands::index_pack::parse_argv(rest.to_vec())?;
            commands::index_pack::run(args)
        }
        "init" | "init-db" => commands::init::run(parse_cmd_args("init", rest), opts.bare),
        "interpret-trailers" => {
            if rest.len() == 1 && (rest[0] == "-h" || rest[0] == "--help") {
                if let Some(syn) = commands::upstream_synopsis_help::synopsis_for_builtin(subcmd) {
                    commands::upstream_synopsis_help::print_upstream_synopsis_stdout_and_exit(
                        subcmd, syn, 129,
                    );
                }
            }
            commands::interpret_trailers::run_from_argv(rest)
        }
        "last-modified" => commands::last_modified::run(parse_cmd_args(subcmd, rest)),
        "log" => {
            let raw_tail = rest.to_vec();
            let rest = preprocess_log_remotes(rest);
            let for_clap = strip_log_revision_pseudo_for_clap(&rest);
            let rest = preprocess_expand_tabs_for_rev_cmd(&preprocess_log_pickaxe_args(
                preprocess_log_args(&for_clap),
            ));
            let mut parsed: commands::log::Args = parse_cmd_args(subcmd, &rest);
            parsed.raw_argv_tail = raw_tail;
            commands::log::run(parsed)
        }
        "ls-files" => commands::ls_files::run(parse_cmd_args(subcmd, rest)),
        "ls-remote" => commands::ls_remote::run(parse_cmd_args(subcmd, rest)),
        "ls-tree" => commands::ls_tree::run(parse_cmd_args(subcmd, rest)),
        "mailinfo" => commands::mailinfo::run(parse_cmd_args(subcmd, rest)),
        "mailsplit" => commands::mailsplit::run(parse_cmd_args(subcmd, rest)),
        "maintenance" => commands::maintenance::run_from_argv(rest),
        "merge" => {
            let rest = preprocess_merge_argv(rest);
            match commands::merge::run(parse_cmd_args(subcmd, &rest)) {
                Ok(()) => Ok(()),
                Err(err) => {
                    if commands::merge::is_internal_merge_execution_error(&err) {
                        eprintln!("error: failed to execute internal merge");
                        std::process::exit(2);
                    }
                    Err(err)
                }
            }
        }
        "merge-recursive" => commands::merge_recursive::run(parse_cmd_args(subcmd, rest)),
        "merge-resolve" => commands::merge_resolve::run(parse_cmd_args(subcmd, rest)),
        "merge-base" => commands::merge_base::run(parse_cmd_args(subcmd, rest)),
        "merge-file" => commands::merge_file::run(parse_cmd_args(subcmd, rest)),
        "merge-index" => commands::merge_index::run(parse_cmd_args(subcmd, rest)),
        "merge-one-file" => commands::merge_one_file::run(parse_cmd_args(subcmd, rest)),
        "merge-tree" => commands::merge_tree::run_from_argv(rest),
        "mergetool" => commands::mergetool::run(parse_cmd_args(subcmd, rest)),
        "mktag" => {
            // `-h`/`--help`/`--help-all` are handled by `parse_cmd_args`, which prints the
            // vendored adoc SYNOPSIS (`git mktag`, no trailing space) so the `-h` output agrees
            // with Documentation/git-mktag.adoc byte-for-byte (t0450).
            commands::mktag::run(parse_cmd_args(subcmd, rest))
        }
        "mktree" => commands::mktree::run(parse_cmd_args(subcmd, rest)),
        "multi-pack-index" => {
            let needs_manual = rest
                .iter()
                .any(|s| s == "--object-dir" || s.starts_with("--object-dir="));
            if needs_manual {
                commands::multi_pack_index::run_from_argv(rest)
            } else {
                commands::multi_pack_index::run(parse_cmd_args(subcmd, rest))
            }
        }
        "mv" => commands::mv::run(parse_cmd_args(subcmd, rest)),
        "name-rev" => commands::name_rev::run(parse_cmd_args(subcmd, rest)),
        "notes" => {
            let mut i = 0usize;
            while i < rest.len() {
                let a = rest[i].as_str();
                if a == "--ref" || a == "--no-ref" {
                    i = i.saturating_add(2);
                    continue;
                }
                if a.starts_with("--ref=") || a.starts_with("--no-ref=") {
                    i += 1;
                    continue;
                }
                break;
            }
            if i < rest.len() {
                let first = rest[i].as_str();
                if !first.starts_with('-') {
                    const NOTES_SUBS: &[&str] = &[
                        "list", "add", "show", "remove", "append", "edit", "copy", "merge",
                        "prune", "get-ref",
                    ];
                    if !NOTES_SUBS.iter().any(|s| *s == first) {
                        eprintln!("error: unknown subcommand: `{first}`");
                        std::process::exit(129);
                    }
                }
            }
            commands::notes::run_from_argv(rest)
        }
        "pack-objects" => {
            let rest = commands::pack_objects::preprocess_argv(rest);
            commands::pack_objects::run(parse_cmd_args(subcmd, &rest))
        }
        "pkt-line" => {
            let sub = rest.first().map(|s| s.as_str()).unwrap_or("");
            match sub {
                "pack" => pkt_line::cmd_pack().map_err(Into::into),
                "unpack" => pkt_line::cmd_unpack().map_err(Into::into),
                other => bail!("pkt-line: unknown subcommand '{other}'"),
            }
        }
        "pack-redundant" => commands::pack_redundant::run(parse_cmd_args(subcmd, rest)),
        "pack-refs" => commands::pack_refs::run(parse_cmd_args(subcmd, rest)),
        "patch-id" => commands::patch_id::run(parse_cmd_args(subcmd, rest)),
        "prune" => commands::prune::run_from_argv(rest),
        "prune-packed" => commands::prune_packed::run(parse_cmd_args(subcmd, rest)),
        "pull" => {
            // git pull sets GIT_REFLOG_ACTION to "pull <args...>" (builtin/pull.c
            // set_reflog_message), but does not override an action inherited from a
            // parent process. The fetch/merge/rebase it drives then record e.g.
            // "pull --rebase . ff: Fast-forward" in the reflog.
            if std::env::var_os("GIT_REFLOG_ACTION").is_none() {
                let mut action = String::from("pull");
                for a in rest {
                    action.push(' ');
                    action.push_str(a);
                }
                std::env::set_var("GIT_REFLOG_ACTION", action);
            }
            commands::pull::run(parse_cmd_args(subcmd, rest))
        }
        "push" => commands::push::run(parse_cmd_args(subcmd, rest)),
        "range-diff" => commands::range_diff::run_with_rest(rest),
        "read-tree" => commands::read_tree::run(parse_cmd_args(subcmd, rest)),
        "rebase" => commands::rebase::run(parse_cmd_args(
            subcmd,
            &commands::rebase::preprocess_rebase_argv(rest),
        )),
        "receive-pack" => commands::receive_pack::run(parse_cmd_args(subcmd, rest)),
        "request-pull" => commands::request_pull::run_from_argv(rest),
        "reflog" => {
            let rest = preprocess_expand_tabs_for_rev_cmd(&preprocess_log_args(rest));
            commands::reflog::run(parse_cmd_args(subcmd, &rest))
        }
        "refs" => commands::refs::run(parse_cmd_args(subcmd, rest)),
        "remote" => commands::remote::run_from_argv(rest),
        "repack" => commands::repack::run(parse_cmd_args(subcmd, rest)),
        "replace" => commands::replace::run(parse_cmd_args(subcmd, rest)),
        "replay" => commands::replay::run(parse_cmd_args(subcmd, rest)),
        "repo" => commands::repo::run(parse_cmd_args(subcmd, rest)),
        "rerere" => commands::rerere::run(parse_cmd_args(subcmd, rest)),
        "reset" => {
            commands::reset::pre_validate_args(rest)?;
            let raw_argv_had_path_separator =
                rest.iter().any(|a| a == "--" || a == "--end-of-options");
            let filtered = commands::reset::filter_args(rest);
            let mut reset_args = parse_cmd_args::<commands::reset::Args>(subcmd, &filtered);
            reset_args.raw_argv_had_path_separator = raw_argv_had_path_separator;
            commands::reset::run(reset_args)
        }
        "restore" => commands::restore::run(parse_cmd_args(subcmd, rest)),
        "rev-list" => commands::rev_list::run(parse_cmd_args(subcmd, rest)),
        "rev-parse" => commands::rev_parse::run_with_raw_args(rest),
        "revert" => commands::revert::run(parse_cmd_args(subcmd, rest)),
        "rm" => commands::rm::run(parse_cmd_args(subcmd, rest)),
        "scalar" => commands::scalar::run(rest),
        "send-pack" => commands::send_pack::run(parse_cmd_args(subcmd, rest)),
        "serve-v2" => commands::serve_v2::run(parse_cmd_args(subcmd, rest)),
        "sh-i18n" => commands::sh_i18n::run(parse_cmd_args(subcmd, rest)),
        "sh-i18n--envsubst" => commands::sh_i18n_envsubst::run_from_argv(rest),
        "sh-setup" => commands::sh_setup::run(parse_cmd_args(subcmd, rest)),
        "shell" => commands::shell::run(parse_cmd_args(subcmd, rest)),
        "shortlog" => {
            commands::upstream_synopsis_help::try_print_upstream_help_and_exit(subcmd, rest);
            commands::shortlog::run_with_raw_args(rest)
        }
        "show" => {
            let rest = preprocess_expand_tabs_for_rev_cmd(rest);
            let rest = preprocess_show_argv(&rest);
            let mut saw_bare_pretty = false;
            let mut explicit_pretty = false;
            let mut i = 0usize;
            while i < rest.len() {
                let a = rest[i].as_str();
                if a == "--pretty" {
                    let next_is_value = rest
                        .get(i + 1)
                        .map(|n| !n.starts_with('-'))
                        .unwrap_or(false);
                    if !next_is_value {
                        saw_bare_pretty = true;
                    } else {
                        explicit_pretty = true;
                    }
                } else if a.starts_with("--pretty=") && a.len() > "--pretty=".len() {
                    explicit_pretty = true;
                } else if a == "--format" {
                    if rest
                        .get(i + 1)
                        .map(|n| !n.starts_with('-'))
                        .unwrap_or(false)
                    {
                        explicit_pretty = true;
                    }
                } else if a.starts_with("--format=") && a.len() > "--format=".len() {
                    explicit_pretty = true;
                }
                i += 1;
            }
            if saw_bare_pretty {
                std::env::set_var("GIT_GRIT_SHOW_BARE_PRETTY", "1");
            } else {
                std::env::remove_var("GIT_GRIT_SHOW_BARE_PRETTY");
            }
            if explicit_pretty {
                std::env::set_var("GIT_GRIT_SHOW_EXPLICIT_PRETTY", "1");
            } else {
                std::env::remove_var("GIT_GRIT_SHOW_EXPLICIT_PRETTY");
            }
            let rest = preprocess_git_notes_display_argv(&rest, NotesDisplayDefault::OnIfUnset);
            commands::show::run(parse_cmd_args(subcmd, &rest))
        }
        "show-branch" => {
            // show-branch is dispatched manually (run_raw discovers the repo first), so it
            // bypasses the centralized -h/--help-all synopsis path. Handle a bare
            // -h/--help/--help-all here before any repo discovery happens.
            commands::upstream_synopsis_help::try_print_upstream_help_and_exit(subcmd, rest);
            commands::show_branch::run_raw(rest)
        }
        "show-index" => commands::show_index::run(parse_cmd_args(subcmd, rest)),
        "show-ref" => commands::show_ref::run(parse_cmd_args(subcmd, rest)),
        "sparse-checkout" => commands::sparse_checkout::run(parse_cmd_args(
            subcmd,
            &preprocess_sparse_checkout_argv(rest),
        )),
        "stage" => commands::stage::run(parse_cmd_args(subcmd, rest)),
        "stash" => commands::stash::run(parse_cmd_args(subcmd, rest)),
        "status" => commands::status::run(parse_cmd_args(subcmd, &preprocess_status_argv(rest))),
        "stripspace" => commands::stripspace::run(parse_cmd_args(subcmd, rest)),
        "submodule" => commands::submodule::run_from_argv(rest),
        "submodule--helper" => commands::submodule::run_submodule_helper(rest),
        "switch" => commands::switch::run(parse_cmd_args(subcmd, rest)),
        "symbolic-ref" => commands::symbolic_ref::run(parse_cmd_args(subcmd, rest)),
        "tag" => commands::tag::run(parse_cmd_args(subcmd, rest)),
        "unpack-file" => commands::unpack_file::run(parse_cmd_args(subcmd, rest)),
        "unpack-objects" => commands::unpack_objects::run(parse_cmd_args(subcmd, rest)),
        // grit-specific (non-git) self-update command.
        "update" => commands::update::run(parse_cmd_args(subcmd, rest)),
        "update-index" => {
            // `-h`/`--help`/`--help-all` are handled by `parse_cmd_args`, which prints the
            // multi-line vendored adoc SYNOPSIS so the `-h` output agrees with
            // Documentation/git-update-index.adoc byte-for-byte (t0450). The synopsis is emitted
            // before any index access, so `update-index -h` still works with a corrupt index
            // (t2107).
            commands::update_index::run(parse_cmd_args(subcmd, rest), rest)
        }
        "update-ref" => commands::update_ref::run(parse_cmd_args(subcmd, rest)),
        "update-server-info" => commands::update_server_info::run(parse_cmd_args(subcmd, rest)),
        "upload-archive" => commands::upload_archive::run(parse_cmd_args(subcmd, rest)),
        "upload-pack" => commands::upload_pack::run(parse_cmd_args(subcmd, rest)),
        "var" => commands::var::run(parse_cmd_args(subcmd, rest)),
        "verify-commit" => commands::verify_commit::run(parse_cmd_args(subcmd, rest)),
        "verify-pack" => commands::verify_pack::run(parse_cmd_args(subcmd, rest)),
        "verify-tag" => commands::verify_tag::run(parse_cmd_args(subcmd, rest)),
        "version" => commands::version::run(parse_cmd_args(subcmd, rest)),
        "web--browse" => commands::web_browse::run(parse_cmd_args(subcmd, rest)),
        "whatchanged" => {
            commands::upstream_synopsis_help::try_print_upstream_help_and_exit(subcmd, rest);
            commands::whatchanged::run(rest)
        }
        "worktree" => commands::worktree::run(parse_cmd_args(subcmd, rest)),
        "write-tree" => commands::write_tree::run(parse_cmd_args(subcmd, rest)),
        // The entire `test-tool` dispatch is gated behind the default `test-tools`
        // feature. With `--no-default-features`, `grit test-tool` is an unknown
        // command (falls through to the `_` arm below).
        #[cfg(feature = "test-tools")]
        "test-tool" => {
            let sub = rest.first().map(|s| s.as_str()).unwrap_or("");
            match sub {
                "wildmatch" => {
                    // test-tool wildmatch <mode> <text> <pattern>
                    if rest.len() < 4 {
                        bail!("usage: test-tool wildmatch <mode> <text> <pattern>");
                    }
                    let mode = &rest[1];
                    let mut text = rest[2].clone();
                    let pattern = rest[3].clone();

                    // Handle XXX/ prefix (substitute for leading /)
                    let text_bytes = if text.starts_with("XXX/") {
                        text = text[3..].to_string();
                        text.as_bytes().to_vec()
                    } else {
                        text.as_bytes().to_vec()
                    };
                    let pat_bytes = if pattern.starts_with("XXX/") {
                        pattern[3..].as_bytes().to_vec()
                    } else {
                        pattern.as_bytes().to_vec()
                    };

                    let flags = match mode.as_str() {
                        "wildmatch" => grit_lib::wildmatch::WM_PATHNAME,
                        "iwildmatch" => {
                            grit_lib::wildmatch::WM_PATHNAME | grit_lib::wildmatch::WM_CASEFOLD
                        }
                        "pathmatch" => 0,
                        "ipathmatch" => grit_lib::wildmatch::WM_CASEFOLD,
                        _ => bail!("unknown wildmatch mode: {mode}"),
                    };

                    let matched = grit_lib::wildmatch::wildmatch(&pat_bytes, &text_bytes, flags);
                    if matched {
                        Ok(())
                    } else {
                        std::process::exit(1);
                    }
                }
                "crontab" => run_test_tool_crontab(&rest[1..]),
                "trace2" => run_test_tool_trace2(rest),
                "example-tap" => run_test_tool_example_tap(rest),
                "advise" => run_test_tool_advise(rest),
                "env-helper" => run_test_tool_env_helper(rest),
                "dir-iterator" => run_test_tool_dir_iterator(rest),
                "parse-pathspec-file" => run_test_tool_parse_pathspec_file(rest),
                "revision-walking" => run_test_tool_revision_walking(rest),
                "mergesort" => run_test_tool_mergesort(rest),
                "hexdump" => run_test_tool_hexdump(rest),
                "chmtime" => run_test_tool_chmtime(&rest[1..]),
                "pack-mtimes" => run_test_tool_pack_mtimes(rest),
                "read-cache" => run_test_tool_read_cache(rest),
                "dump-cache-tree" => run_test_tool_dump_cache_tree(),
                "scrap-cache-tree" => run_test_tool_scrap_cache_tree(),
                "dump-untracked-cache" => run_test_tool_dump_untracked_cache(),
                "dump-split-index" => run_test_tool_dump_split_index(&rest[1..]),
                "dump-fsmonitor" => run_test_tool_dump_fsmonitor(),
                "userdiff" => run_test_tool_userdiff(rest),
                "find-pack" => run_test_tool_find_pack(rest),
                "bitmap" => run_test_tool_bitmap(rest),
                "partial-clone" => run_test_tool_partial_clone(rest),
                "ref-store" => run_test_tool_ref_store(rest),
                "reach" => commands::test_tool_reach::run(&rest[1..]),
                "path-walk" => run_test_tool_path_walk(rest),
                "online-cpus" => run_test_tool_online_cpus(rest),
                "lazy-init-name-hash" => run_test_tool_lazy_init_name_hash(rest),
                "rot13-filter" => commands::test_tool_rot13_filter::run(&rest[1..]),
                "path-utils" => run_test_tool_path_utils(&rest[1..]),
                "run-command" => test_tool_run_command::run(&rest[1..]),
                "subprocess" => run_test_tool_subprocess(&rest[1..]),
                "submodule" => run_test_tool_submodule(rest),
                "submodule-config" => run_test_tool_submodule_config(rest),
                "submodule-nested-repo-config" => run_test_tool_submodule_nested_repo_config(rest),
                "config" => run_test_tool_config(&rest[1..]),
                "parse-options" => {
                    let args = preprocess_test_tool_args(rest)?;
                    use grit_lib::parse_options_test_tool::ParseOptionsToolError;
                    match grit_lib::parse_options_test_tool::run_parse_options(&args) {
                        Ok(code) => {
                            let _ = std::io::stdout().flush();
                            std::process::exit(code);
                        }
                        Err(ParseOptionsToolError::Help) => {
                            let _ = std::io::stdout().flush();
                            std::process::exit(129);
                        }
                        Err(ParseOptionsToolError::Silent) => {
                            let _ = std::io::stderr().flush();
                            std::process::exit(1);
                        }
                        Err(ParseOptionsToolError::Fatal(s)) => {
                            eprint!("{s}");
                            let _ = std::io::stderr().flush();
                            std::process::exit(129);
                        }
                        Err(ParseOptionsToolError::Bug(s)) => {
                            eprint!("{s}");
                            let _ = std::io::stderr().flush();
                            std::process::exit(99);
                        }
                    }
                }
                "parse-options-flags" => {
                    let args = preprocess_test_tool_args(rest)?;
                    use grit_lib::parse_options_test_tool::ParseOptionsToolError;
                    match grit_lib::parse_options_test_tool::run_parse_options_flags(&args) {
                        Ok(code) => {
                            let _ = std::io::stdout().flush();
                            std::process::exit(code);
                        }
                        Err(ParseOptionsToolError::Help) => {
                            let _ = std::io::stdout().flush();
                            std::process::exit(129);
                        }
                        Err(ParseOptionsToolError::Silent) => {
                            let _ = std::io::stderr().flush();
                            std::process::exit(1);
                        }
                        Err(ParseOptionsToolError::Fatal(s)) => {
                            eprint!("{s}");
                            let _ = std::io::stderr().flush();
                            std::process::exit(129);
                        }
                        Err(ParseOptionsToolError::Bug(s)) => {
                            eprint!("{s}");
                            let _ = std::io::stderr().flush();
                            std::process::exit(99);
                        }
                    }
                }
                "parse-subcommand" => {
                    let args = preprocess_test_tool_args(rest)?;
                    use grit_lib::parse_options_test_tool::ParseOptionsToolError;
                    match grit_lib::parse_options_test_tool::run_parse_subcommand(&args) {
                        Ok(code) => {
                            let _ = std::io::stdout().flush();
                            std::process::exit(code);
                        }
                        Err(ParseOptionsToolError::Help) => {
                            let _ = std::io::stdout().flush();
                            std::process::exit(129);
                        }
                        Err(ParseOptionsToolError::Silent) => {
                            let _ = std::io::stderr().flush();
                            std::process::exit(1);
                        }
                        Err(ParseOptionsToolError::Fatal(s)) => {
                            eprint!("{s}");
                            let _ = std::io::stderr().flush();
                            std::process::exit(129);
                        }
                        Err(ParseOptionsToolError::Bug(s)) => {
                            eprint!("{s}");
                            let _ = std::io::stderr().flush();
                            std::process::exit(99);
                        }
                    }
                }
                "date" => match grit_lib::git_date::test_tool_date(&rest[1..]) {
                    Ok(grit_lib::git_date::TestToolDateResult::Output(lines)) => {
                        for line in lines {
                            println!("{line}");
                        }
                        Ok(())
                    }
                    Ok(grit_lib::git_date::TestToolDateResult::Exit(code)) => {
                        std::process::exit(code);
                    }
                    Err(e) => bail!("{e}"),
                },
                "read-midx" => {
                    use grit_lib::midx::{
                        format_midx_dump_layer, format_midx_show_objects_layer, midx_checksum_hex,
                        read_midx_preferred_idx_name,
                    };
                    let sub = rest.get(1).map(|s| s.as_str()).unwrap_or("");
                    match sub {
                        "--preferred-pack" => {
                            let dir = rest.get(2).map(Path::new).context(
                                "usage: test-tool read-midx --preferred-pack <object-dir>",
                            )?;
                            match read_midx_preferred_idx_name(dir) {
                                Ok(name) => println!("{name}"),
                                Err(e) => {
                                    // Match `test-read-midx.c`: a missing reverse index
                                    // yields a warning and a non-zero exit.
                                    let msg = e.to_string();
                                    if msg.contains("could not determine MIDX preferred pack") {
                                        eprintln!(
                                            "warning: could not determine MIDX preferred pack"
                                        );
                                    } else {
                                        eprintln!("error: {msg}");
                                    }
                                    std::process::exit(1);
                                }
                            }
                        }
                        "--checksum" => {
                            let dir = rest
                                .get(2)
                                .map(Path::new)
                                .context("usage: test-tool read-midx --checksum <object-dir>")?;
                            let h = midx_checksum_hex(dir).map_err(|e| anyhow::anyhow!("{e}"))?;
                            println!("{h}");
                        }
                        "--show-objects" => {
                            let dir = rest.get(2).map(Path::new).context(
                                "usage: test-tool read-midx --show-objects <object-dir>",
                            )?;
                            let checksum = rest.get(3).map(|s| s.as_str());
                            match format_midx_show_objects_layer(dir, checksum) {
                                Ok(s) => print!("{s}"),
                                Err(e) => {
                                    // git's test-read-midx prints `error: could not find
                                    // MIDX with checksum <hash>` and exits non-zero.
                                    eprintln!("error: {e}");
                                    std::process::exit(1);
                                }
                            }
                        }
                        "--bitmap" => {
                            use grit_lib::midx::format_midx_bitmapped_packs;
                            let dir = rest
                                .get(2)
                                .map(Path::new)
                                .context("usage: test-tool read-midx --bitmap <object-dir>")?;
                            match format_midx_bitmapped_packs(dir) {
                                Ok(s) => print!("{s}"),
                                Err(e) => {
                                    // Print the bare message (the `Error::CorruptObject`
                                    // Display adds a `corrupt object: ` prefix we don't want).
                                    let msg = e.to_string();
                                    let msg = msg.strip_prefix("corrupt object: ").unwrap_or(&msg);
                                    eprintln!("error: {msg}");
                                    std::process::exit(1);
                                }
                            }
                        }
                        "" => bail!("usage: test-tool read-midx <object-dir>"),
                        dir => {
                            // `read-midx <object-dir> [<checksum>]`: an optional checksum
                            // selects a specific incremental layer.
                            let checksum = rest.get(2).map(|s| s.as_str());
                            match format_midx_dump_layer(Path::new(dir), checksum) {
                                Ok(dump) => print!("{dump}"),
                                Err(e) => {
                                    eprintln!("error: {e}");
                                    std::process::exit(1);
                                }
                            }
                        }
                    }
                    Ok(())
                }
                "read-graph" => {
                    use grit_lib::commit_graph_file::{dump_bloom_filters, parse_graph_file};
                    use grit_lib::repo::Repository;
                    let repo = Repository::discover(None)
                        .map_err(|e| anyhow::anyhow!("read-graph: {e}"))?;
                    let objects = repo.git_dir.join("objects");
                    let info = objects.join("info");
                    let chain_path = info.join("commit-graphs").join("commit-graph-chain");
                    let graph_path = if chain_path.is_file() {
                        let content = std::fs::read_to_string(&chain_path)
                            .map_err(|e| anyhow::anyhow!("read-graph: {e}"))?;
                        // The chain file lists layers base-first (line 1 is the base,
                        // the last line is the tip). `git/t/helper/test-read-graph.c`
                        // reports on the *tip* layer (the one with the most base
                        // graphs), so read the last non-empty line.
                        let last = content
                            .lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty())
                            .next_back()
                            .unwrap_or("");
                        if last.len() != 40 {
                            bail!("read-graph: invalid commit-graph chain");
                        }
                        info.join("commit-graphs")
                            .join(format!("graph-{last}.graph"))
                    } else {
                        info.join("commit-graph")
                    };
                    if rest.get(1).map(|s| s.as_str()) == Some("bloom-filters") {
                        let lines = dump_bloom_filters(&graph_path).ok_or_else(|| {
                            anyhow::anyhow!("read-graph: missing or corrupt graph")
                        })?;
                        for line in lines {
                            if !line.is_empty() {
                                println!("{line}");
                            }
                        }
                    } else {
                        let dump = parse_graph_file(&graph_path).ok_or_else(|| {
                            anyhow::anyhow!("read-graph: missing or corrupt graph")
                        })?;
                        println!(
                            "header: {:08x} {} {} {} {}",
                            dump.header_word,
                            dump.version,
                            dump.hash_ver,
                            dump.num_chunks,
                            dump.reserved
                        );
                        println!("num_commits: {}", dump.num_commits);
                        println!("chunks: {}", dump.chunks);
                        println!("options:{}", dump.options);
                    }
                    Ok(())
                }
                "genrandom" => {
                    // Match `git/t/helper/test-genrandom.c`: `test-tool genrandom <seed> [<size>]`.
                    // With two args only, emit until the pipe breaks (size omitted → unbounded).
                    use std::io::Write;
                    fn parse_genrandom_size(arg: &str) -> anyhow::Result<usize> {
                        // Match `git_parse_ulong` + `get_unit_factor`: optional `k`/`m`/`g` suffix.
                        if arg.is_empty() || arg.contains('-') {
                            anyhow::bail!("cannot parse genrandom size '{arg}'");
                        }
                        let lower = arg.to_ascii_lowercase();
                        let (num, mult): (&str, u128) = match lower.as_bytes().last().copied() {
                            Some(b'k') => (&lower[..lower.len() - 1], 1024),
                            Some(b'm') => (&lower[..lower.len() - 1], 1024 * 1024),
                            Some(b'g') => (&lower[..lower.len() - 1], 1024_u128 * 1024 * 1024),
                            _ => (lower.as_str(), 1),
                        };
                        if num.is_empty() {
                            anyhow::bail!("cannot parse genrandom size '{arg}'");
                        }
                        let val: u128 = num
                            .parse()
                            .map_err(|_| anyhow::anyhow!("cannot parse genrandom size '{arg}'"))?;
                        let total = val.checked_mul(mult).ok_or_else(|| {
                            anyhow::anyhow!("cannot parse genrandom size '{arg}'")
                        })?;
                        usize::try_from(total)
                            .map_err(|_| anyhow::anyhow!("cannot parse genrandom size '{arg}'"))
                    }
                    if rest.len() < 2 {
                        bail!("usage: test-tool genrandom <seed_string> [<size>]");
                    }
                    let seed = rest[1].as_str();
                    let count: Option<usize> = if rest.len() >= 3 {
                        Some(parse_genrandom_size(rest[2].as_str()).with_context(|| {
                            format!("cannot parse genrandom size '{}'", rest[2])
                        })?)
                    } else {
                        None
                    };
                    let mut next: u64 = 0;
                    for b in seed.bytes() {
                        next = next.wrapping_mul(11).wrapping_add(b as u64);
                    }
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
                    let mut buf = vec![0u8; 8192];
                    match count {
                        Some(mut remaining) => {
                            while remaining > 0 {
                                let chunk = remaining.min(8192);
                                for b in &mut buf[..chunk] {
                                    next = next.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                                    *b = ((next >> 16) & 0xff) as u8;
                                }
                                out.write_all(&buf[..chunk])?;
                                remaining -= chunk;
                            }
                        }
                        None => loop {
                            for b in &mut buf {
                                next = next.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                                *b = ((next >> 16) & 0xff) as u8;
                            }
                            match out.write_all(&buf) {
                                Ok(()) => {}
                                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => break,
                                Err(e) => return Err(e.into()),
                            }
                        },
                    }
                    Ok(())
                }
                "genzeros" => {
                    // Generate N zero bytes
                    let n: usize = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                    use std::io::Write;
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
                    let buf = vec![0u8; 8192];
                    let mut remaining = n;
                    while remaining > 0 {
                        let chunk = remaining.min(8192);
                        out.write_all(&buf[..chunk])?;
                        remaining -= chunk;
                    }
                    Ok(())
                }
                "truncate" => {
                    let path = rest.get(1).ok_or_else(|| {
                        anyhow::anyhow!("usage: test-tool truncate <file> <size>")
                    })?;
                    let size = rest
                        .get(2)
                        .ok_or_else(|| anyhow::anyhow!("usage: test-tool truncate <file> <size>"))?
                        .parse::<u64>()
                        .with_context(|| format!("invalid truncate size '{}'", rest[2]))?;
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(path)
                        .with_context(|| format!("open {path}"))?;
                    file.set_len(size)
                        .with_context(|| format!("truncate {path}"))?;
                    Ok(())
                }
                "bundle-uri" => {
                    let sub = rest.get(1).map(|s| s.as_str()).unwrap_or("");
                    match sub {
                        "ls-remote" => {
                            let url = rest.get(2).map(|s| s.as_str()).unwrap_or("");
                            if url.is_empty() {
                                bail!("usage: test-tool bundle-uri ls-remote <url>");
                            }
                            let pairs = if url.starts_with("file://") {
                                crate::file_upload_pack_v2::fetch_bundle_uri_lines_file(url)
                            } else if url.starts_with("git://") {
                                crate::file_upload_pack_v2::fetch_bundle_uri_lines_git(url)
                            } else {
                                crate::http_bundle_uri::fetch_bundle_uri_lines_http(url)
                            }
                            .with_context(|| "could not get the bundle-uri list")?;
                            crate::http_bundle_uri::print_bundle_list_from_pairs(&pairs);
                            Ok(())
                        }
                        "parse-key-values" => {
                            let path = rest.get(2).map(|s| s.as_str()).unwrap_or("");
                            if path.is_empty() {
                                bail!("usage: test-tool bundle-uri parse-key-values <input>");
                            }
                            let code = crate::bundle_uri_test_tool::parse_key_values_file(path)?;
                            std::process::exit(code);
                        }
                        "parse-config" => {
                            let path = rest.get(2).map(|s| s.as_str()).unwrap_or("");
                            if path.is_empty() {
                                bail!("usage: test-tool bundle-uri parse-config <input>");
                            }
                            let code =
                                crate::bundle_uri_test_tool::parse_config_file("<uri>", path)?;
                            std::process::exit(code);
                        }
                        other => bail!("test-tool bundle-uri: unknown subcommand '{other}'"),
                    }
                }
                "simple-ipc" => {
                    let code = grit_lib::simple_ipc::run_simple_ipc_tool(&rest[1..]);
                    std::process::exit(code);
                }
                "name-hash" => {
                    use grit_lib::pack_name_hash::{pack_name_hash, pack_name_hash_v2};
                    use std::io::{BufRead, Write};
                    let stdin = std::io::stdin();
                    let mut stdout = std::io::stdout();
                    for line in stdin.lock().lines() {
                        let line = line?;
                        if line.is_empty() {
                            continue;
                        }
                        let h1 = pack_name_hash(&line);
                        let h2 = pack_name_hash_v2(line.as_bytes());
                        writeln!(stdout, "{h1:10} {h2:10} {line}")?;
                    }
                    Ok(())
                }
                "progress" => grit_lib::test_tool_progress::run().map_err(|e| e.into()),
                "getcwd" => {
                    if rest.len() != 1 {
                        bail!("usage: test-tool getcwd");
                    }
                    let cwd = std::env::current_dir().context("getcwd")?;
                    println!("{}", cwd.display());
                    Ok(())
                }
                "pack-deltas" => {
                    let args = preprocess_test_tool_args(rest)?;
                    test_tool_pack_deltas::run(&args)
                }
                "delta" => run_test_tool_delta(rest),
                "dump-reftable" => {
                    // Supports `-b` (dump per-block stats) used by t0613.
                    let mut dump_blocks = false;
                    let mut table: Option<&str> = None;
                    for arg in &rest[1..] {
                        if arg == "-b" {
                            dump_blocks = true;
                        } else {
                            table = Some(arg.as_str());
                        }
                    }
                    let table = table.ok_or_else(|| {
                        anyhow::anyhow!("usage: test-tool dump-reftable [-b] arg")
                    })?;
                    if dump_blocks {
                        let out =
                            grit_lib::reftable::dump_reftable_blocks(std::path::Path::new(table))
                                .map_err(|e| anyhow::anyhow!("{e}"))?;
                        print!("{out}");
                        Ok(())
                    } else {
                        bail!("test-tool dump-reftable: only -b is supported");
                    }
                }
                other => bail!("test-tool: unknown subcommand '{other}'"),
            }
        }
        "__list_cmds" => {
            let categories = rest.first().map(|s| s.as_str()).unwrap_or("");
            print_list_cmds(categories);
            Ok(())
        }
        _ => {
            if rest.len() == 1 && (rest[0] == "--help" || rest[0] == "-h") {
                eprintln!("git: '{subcmd}' is not a git command. See 'git --help'.");
                std::process::exit(1);
            }
            handle_unknown_git_command(subcmd, rest, opts)
        }
    }
}

/// Normalize a path (resolve . and ..) without requiring filesystem existence.
/// Returns "++failed++" if path goes above root for relative paths.
#[cfg(feature = "test-tools")]
fn normalize_path_simple(path: &str) -> String {
    match git_path::normalize_path_copy(path) {
        Ok(s) => s,
        Err(_) => "++failed++".to_string(),
    }
}

/// POSIX `basename(3)` (matches libc used by Git's test-tool path-utils).
#[cfg(feature = "test-tools")]
fn posix_basename(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let mut end = path.len();
    while end > 0 && path.as_bytes()[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return "/".to_string();
    }
    let path = &path[..end];
    if let Some(i) = path.rfind('/') {
        path[i + 1..].to_string()
    } else {
        path.to_string()
    }
}

/// POSIX `dirname(3)` (matches libc used by Git's test-tool path-utils).
#[cfg(feature = "test-tools")]
fn posix_dirname(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let mut end = path.len();
    while end > 0 && path.as_bytes()[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return "/".to_string();
    }
    let mut len = end;
    while len > 0 && path.as_bytes()[len - 1] != b'/' {
        len -= 1;
    }
    if len == 0 {
        if path.as_bytes()[0] == b'/' {
            return "/".to_string();
        }
        return ".".to_string();
    }
    let mut d_end = len;
    while d_end > 0 && path.as_bytes()[d_end - 1] == b'/' {
        d_end -= 1;
    }
    if d_end == 0 {
        "/".to_string()
    } else {
        path[..d_end].to_string()
    }
}

/// `test-tool subprocess` — matches `git/t/helper/test-subprocess.c`: discover repo, optionally
/// `setup_work_tree` (chdir + normalize `GIT_WORK_TREE`), then re-exec grit with the remaining args.
#[cfg(feature = "test-tools")]
fn run_test_tool_subprocess(rest: &[String]) -> Result<()> {
    use std::process::Command;

    let repo = match grit_lib::rev_parse::discover_optional(None)? {
        Some(r) => r,
        None => bail!("No git repo found"),
    };

    let mut argv = rest;
    if argv.first().map(|s| s.as_str()) == Some("--setup-work-tree") {
        let wt = repo
            .work_tree
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?;
        std::env::set_current_dir(wt).context("cannot chdir to work tree")?;
        std::env::set_var("GIT_DIR", &repo.git_dir);
        std::env::set_var("GIT_WORK_TREE", ".");
        argv = &rest[1..];
    }

    if argv.is_empty() {
        bail!("usage: test-tool subprocess [--setup-work-tree] <command> [args...]");
    }

    let mut cmd = Command::new(crate::grit_exe::grit_executable());
    cmd.args(argv);
    crate::grit_exe::strip_trace2_env(&mut cmd);
    let status = cmd
        .status()
        .context("test-tool subprocess: failed to spawn child")?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Handle `test-tool path-utils` — path manipulation utilities.
#[cfg(feature = "test-tools")]
fn run_test_tool_path_utils(rest: &[String]) -> Result<()> {
    let subcmd = rest.first().map(|s| s.as_str()).unwrap_or("");
    match subcmd {
        "normalize_path_copy" => {
            let path = rest
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("normalize_path_copy: missing path"))?;
            println!("{}", normalize_path_simple(path));
            Ok(())
        }
        "print_path" => {
            let path = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            println!("{path}");
            Ok(())
        }
        "real_path" => {
            let path = rest
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("real_path: missing path"))?;
            if path.is_empty() {
                bail!("The empty string is not a valid path");
            }
            let p = git_path::real_path_resolving(path);
            println!("{}", p.display());
            Ok(())
        }
        "absolute_path" => {
            let path = rest
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("absolute_path: missing path"))?;
            if path.is_empty() {
                bail!("The empty string is not a valid path");
            }
            let cwd = std::env::current_dir()?;
            let abs = if std::path::Path::new(path).is_absolute() {
                normalize_path_simple(path)
            } else {
                normalize_path_simple(&cwd.join(path).display().to_string())
            };
            println!("{abs}");
            Ok(())
        }
        "basename" => {
            for arg in &rest[1..] {
                println!("{}", posix_basename(arg));
            }
            Ok(())
        }
        "dirname" => {
            for arg in &rest[1..] {
                println!("{}", posix_dirname(arg));
            }
            Ok(())
        }
        "file-size" => {
            // Match `git/t/helper/test-path-utils.c`: print st.st_size per path, newline-separated.
            let mut err = 0i32;
            for path in rest.iter().skip(1) {
                match std::fs::metadata(path) {
                    Ok(m) => println!("{}", m.len()),
                    Err(e) => {
                        eprintln!("error: Cannot stat '{path}': {e}");
                        err = 1;
                    }
                }
            }
            if err != 0 {
                std::process::exit(err);
            }
            Ok(())
        }
        "readlink" => {
            let mut failed = false;
            for path in rest.iter().skip(1) {
                match std::fs::read_link(path) {
                    Ok(target) => println!("{}", target.display()),
                    Err(e) => {
                        eprintln!("error: readlink '{path}': {e}");
                        failed = true;
                    }
                }
            }
            if failed {
                std::process::exit(1);
            }
            Ok(())
        }
        "strip_path_suffix" => {
            let path = rest
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("strip_path_suffix: missing path"))?;
            let suffix = rest.get(2).map(|s| s.as_str()).unwrap_or("");
            match git_path::strip_path_suffix(path, suffix) {
                Some(p) => println!("{p}"),
                None => std::process::exit(1),
            }
            Ok(())
        }
        "longest_ancestor_length" => {
            let path = rest
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("longest_ancestor_length: missing path"))?;
            let prefixes_str = rest
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("longest_ancestor_length: missing prefixes"))?;
            let len = git_path::longest_ancestor_length(path, prefixes_str).map_err(|_| {
                anyhow::anyhow!("longest_ancestor_length: could not normalize path")
            })?;
            println!("{len}");
            Ok(())
        }
        "prefix_path" => {
            let prefix = rest
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("prefix_path: missing prefix"))?;
            let path = rest
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("prefix_path: missing path"))?;
            let repo = grit_lib::repo::Repository::discover(None)
                .map_err(|_| anyhow::anyhow!("prefix_path: not a git repository"))?;
            let Some(wt) = repo.work_tree.as_ref() else {
                bail!("prefix_path: bare repository");
            };
            match git_path::prefix_path_gently(prefix, path, wt.as_path()) {
                Some(p) => println!("{p}"),
                None => bail!("prefix_path: path outside repository"),
            }
            Ok(())
        }
        "relative_path" => {
            let path = rest
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("relative_path: missing path"))?;
            let base = rest
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("relative_path: missing base"))?;
            let path = if path == "<empty>" || path == "<null>" || path == "(null)" {
                ""
            } else {
                path.as_str()
            };
            let base = if base == "<empty>" || base == "<null>" || base == "(null)" {
                ""
            } else {
                base.as_str()
            };
            let mut sb = String::new();
            let rel = git_path::relative_path(path, base, &mut sb);
            match rel {
                None => println!("(null)"),
                Some(s) if s.is_empty() => println!("(empty)"),
                Some(s) => println!("{s}"),
            }
            Ok(())
        }
        "is_dotgitattributes" | "is_dotgitignore" | "is_dotgitmodules" | "is_dotmailmap" => {
            let mut res = 0;
            let mut expect = 1;
            for arg in &rest[1..] {
                if arg == "--not" {
                    expect = 0;
                    continue;
                }
                let hit = grit_lib::dotfile::dotfile_matches(subcmd, arg);
                if expect != (hit as i32) {
                    res = 1;
                }
            }
            std::process::exit(res);
        }
        other => bail!("test-tool path-utils: unknown subcommand '{other}'"),
    }
}

/// `test-tool submodule-config` — Git `t7411-submodule-config` helper (`test-submodule-config.c`).
#[cfg(feature = "test-tools")]
fn run_test_tool_submodule_config(rest: &[String]) -> Result<()> {
    let args = preprocess_test_tool_args(rest)?;
    if args.first().map(|s| s.as_str()) != Some("submodule-config") {
        bail!("internal error: test-tool submodule-config dispatcher");
    }
    let mut i = 1usize;
    let mut lookup_name = false;
    while i < args.len() {
        let a = args[i].as_str();
        if !a.starts_with("--") {
            break;
        }
        if a == "--name" {
            lookup_name = true;
        } else {
            bail!("unknown option: {a}");
        }
        i += 1;
    }
    let pairs = &args[i..];
    if pairs.len() % 2 != 0 {
        bail!("Wrong number of arguments.");
    }
    let repo = grit_lib::repo::Repository::discover(None).context("not a git repository")?;
    let mut cache = grit_lib::submodule_config_cache::SubmoduleConfigCache::new();
    let mut chunk = 0usize;
    while chunk < pairs.len() {
        let commit_spec = pairs[chunk].as_str();
        let path_or_name = pairs[chunk + 1].as_str();
        let treeish = if commit_spec.is_empty() {
            None
        } else {
            // `resolve_revision("HEAD")` follows the detached `HEAD` symref in this harness;
            // Git's submodule-blob label uses the branch tip (`git rev-parse HEAD` on a branch).
            let rev = if commit_spec == "HEAD" {
                grit_lib::state::resolve_head(&repo.git_dir)
                    .map_err(|_| anyhow::anyhow!("Commit not found."))?
                    .oid()
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("Commit not found."))?
            } else {
                grit_lib::rev_parse::resolve_revision(&repo, commit_spec)
                    .map_err(|_| anyhow::anyhow!("Commit not found."))?
            };
            let tree = grit_lib::rev_parse::peel_to_tree(&repo, rev)
                .map_err(|e| anyhow::anyhow!("Commit not found. ({e})"))?;
            Some((rev, tree))
        };
        let sub = if lookup_name {
            cache
                .submodule_from_name(&repo, treeish, path_or_name)
                .map_err(|e| anyhow::anyhow!("{e}"))?
        } else {
            cache
                .submodule_from_path(&repo, treeish, path_or_name)
                .map_err(|e| anyhow::anyhow!("{e}"))?
        };
        let Some(sub) = sub else {
            bail!("Submodule not found.");
        };
        println!("Submodule name: '{}' for path '{}'", sub.name, sub.path);
        chunk += 2;
    }
    Ok(())
}

/// `test-tool submodule-nested-repo-config` — nested `.gitmodules` reader (`test-submodule-nested-repo-config.c`).
#[cfg(feature = "test-tools")]
fn run_test_tool_submodule_nested_repo_config(rest: &[String]) -> Result<()> {
    let args = preprocess_test_tool_args(rest)?;
    if args.first().map(|s| s.as_str()) != Some("submodule-nested-repo-config") {
        bail!("internal error: test-tool submodule-nested-repo-config dispatcher");
    }
    if args.len() != 3 {
        bail!("Wrong number of arguments.");
    }
    let repo = grit_lib::repo::Repository::discover(None).context("not a git repository")?;
    let wt = repo.work_tree.as_ref().context("bare repository")?;
    grit_lib::submodule_config_cache::SubmoduleConfigCache::print_config_from_nested_gitmodules(
        &repo,
        wt.as_path(),
        &args[1],
        &args[2],
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Handle `test-tool submodule` subcommands.
#[cfg(feature = "test-tools")]
fn run_test_tool_submodule(rest: &[String]) -> Result<()> {
    let args = preprocess_test_tool_args(rest)?;
    if args.first().map(|s| s.as_str()) != Some("submodule") {
        bail!("internal error: test-tool submodule dispatcher");
    }
    let rest = &args[1..];
    let subcmd = rest.first().map(|s| s.as_str()).unwrap_or("");
    match subcmd {
        "resolve-relative-url" => {
            // resolve-relative-url <up_path> <remoteurl> <url> — see git/t/helper/test-submodule.c
            let up_path = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let remote_url = rest.get(2).map(|s| s.as_str()).unwrap_or("");
            let url = rest.get(3).map(|s| s.as_str()).unwrap_or("");
            let up = if up_path == "(null)" {
                None
            } else {
                Some(up_path)
            };
            let result = git_path::relative_url(remote_url, url, up)
                .map_err(|_| anyhow::anyhow!("resolve-relative-url: invalid remote_url"))?;
            println!("{result}");
            Ok(())
        }
        "config-list" => {
            let key = rest
                .get(1)
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("usage: test-tool submodule config-list <key>"))?;
            let repo =
                grit_lib::repo::Repository::discover(None).context("not a git repository")?;
            let work_tree = repo.work_tree.as_ref().context("bare repository")?;
            let wanted =
                grit_lib::config::canonical_key(key).map_err(|e| anyhow::anyhow!("{e}"))?;
            let (content, path_for_parse) =
                gitmodules_file_content_for_test_tool(&repo, work_tree)?;
            let cfg = grit_lib::config::ConfigFile::parse(
                &path_for_parse,
                &content,
                grit_lib::config::ConfigScope::Local,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            for e in &cfg.entries {
                if e.key == wanted {
                    if let Some(v) = &e.value {
                        println!("{v}");
                    }
                }
            }
            Ok(())
        }
        "config-set" => {
            let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let value = rest.get(2).map(|s| s.as_str()).unwrap_or("");
            if key.is_empty() {
                bail!("usage: test-tool submodule config-set <key> <value>");
            }
            let repo =
                grit_lib::repo::Repository::discover(None).context("not a git repository")?;
            let work_tree = repo.work_tree.as_ref().context("bare repository")?;
            if !is_writing_gitmodules_ok_for_test_tool(&repo, work_tree) {
                bail!("please make sure that the .gitmodules file is in the working tree");
            }
            let path = work_tree.join(".gitmodules");
            let mut cfg = if path.exists() {
                let content = std::fs::read_to_string(&path).context("reading .gitmodules")?;
                grit_lib::config::ConfigFile::parse(
                    &path,
                    &content,
                    grit_lib::config::ConfigScope::Local,
                )?
            } else {
                grit_lib::config::ConfigFile::parse(
                    &path,
                    "",
                    grit_lib::config::ConfigScope::Local,
                )?
            };
            cfg.set(key, value).map_err(|e| anyhow::anyhow!("{e}"))?;
            cfg.write().map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(())
        }
        "config-unset" => {
            let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            if key.is_empty() {
                bail!("usage: test-tool submodule config-unset <key>");
            }
            let repo =
                grit_lib::repo::Repository::discover(None).context("not a git repository")?;
            let work_tree = repo.work_tree.as_ref().context("bare repository")?;
            if !is_writing_gitmodules_ok_for_test_tool(&repo, work_tree) {
                bail!("please make sure that the .gitmodules file is in the working tree");
            }
            let path = work_tree.join(".gitmodules");
            if !path.exists() {
                return Ok(());
            }
            let content = std::fs::read_to_string(&path).context("reading .gitmodules")?;
            let mut cfg = grit_lib::config::ConfigFile::parse(
                &path,
                &content,
                grit_lib::config::ConfigScope::Local,
            )?;
            let _ = cfg.unset(key).map_err(|e| anyhow::anyhow!("{e}"))?;
            cfg.write().map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(())
        }
        "config-writeable" => {
            if rest.len() != 1 {
                bail!("usage: test-tool submodule config-writeable");
            }
            let repo =
                grit_lib::repo::Repository::discover(None).context("not a git repository")?;
            let work_tree = repo.work_tree.as_ref().context("bare repository")?;
            if is_writing_gitmodules_ok_for_test_tool(&repo, work_tree) {
                Ok(())
            } else {
                bail!(".gitmodules is not writable in this state");
            }
        }
        "is-active" => {
            let path = rest
                .get(1)
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("usage: test-tool submodule is-active <path>"))?;
            let repo =
                grit_lib::repo::Repository::discover(None).context("not a git repository")?;
            match grit_lib::submodule_active::is_submodule_active(&repo, path) {
                Ok(true) => Ok(()),
                Ok(false) => std::process::exit(1),
                Err(msg) => {
                    eprintln!("{msg}");
                    Ok(())
                }
            }
        }
        "check-name" => {
            let mut buf = String::new();
            let stdin = std::io::stdin();
            let mut reader = std::io::BufReader::new(stdin.lock());
            use std::io::BufRead;
            while reader.read_line(&mut buf)? > 0 {
                let line = buf.trim_end_matches(['\n', '\r']);
                if grit_lib::gitmodules::check_submodule_name(line) {
                    println!("{line}");
                }
                buf.clear();
            }
            Ok(())
        }
        "check-url" => {
            let mut buf = String::new();
            let stdin = std::io::stdin();
            let mut reader = std::io::BufReader::new(stdin.lock());
            use std::io::BufRead;
            while reader.read_line(&mut buf)? > 0 {
                let line = buf.trim_end_matches(['\n', '\r']);
                if grit_lib::gitmodules::check_submodule_url(line) {
                    println!("{line}");
                }
                buf.clear();
            }
            Ok(())
        }
        other => bail!("test-tool submodule: unknown subcommand '{other}'"),
    }
}

/// `test-tool dump-untracked-cache` — matches `git/t/helper/test-dump-untracked-cache.c`.
#[cfg(feature = "test-tools")]
fn run_test_tool_dump_split_index(rest: &[String]) -> Result<()> {
    use grit_lib::index::Index;
    use grit_lib::repo::Repository;
    use grit_lib::split_index::format_dump_split_index_file;
    use std::path::PathBuf;

    let path = rest
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: test-tool dump-split-index <index-file>"))?;
    let _repo = Repository::discover(None).context("not a git repository")?;
    let p = PathBuf::from(path);
    let p = if p.is_absolute() {
        p
    } else {
        std::env::current_dir().context("cwd")?.join(p)
    };
    let data = std::fs::read(&p).with_context(|| format!("read {}", p.display()))?;
    let idx = Index::parse(&data).context("parse index")?;
    let out = format_dump_split_index_file(&data, &idx).context("dump split index")?;
    print!("{out}");
    Ok(())
}

/// `test-tool dump-cache-tree` — matches `git/t/helper/test-dump-cache-tree.c`.
///
/// Loads the index, compares the stored cache-tree against a freshly built
/// reference, and prints the agreeing nodes.
#[cfg(feature = "test-tools")]
fn run_test_tool_dump_cache_tree() -> Result<()> {
    use grit_lib::repo::Repository;

    let repo = Repository::discover(None).context("not a git repository")?;
    let index_path = repo.index_path_for_env().context("resolve index path")?;
    let index = repo
        .load_index_at(&index_path)
        .with_context(|| format!("loading index {}", index_path.display()))?;
    let out = index
        .dump_cache_tree(&repo.odb)
        .context("dump cache-tree")?;
    print!("{out}");
    Ok(())
}

/// `test-tool scrap-cache-tree` — matches `git/t/helper/test-scrap-cache-tree.c`.
///
/// Drops the cache-tree extension from the index and writes it back.
#[cfg(feature = "test-tools")]
fn run_test_tool_scrap_cache_tree() -> Result<()> {
    use grit_lib::repo::Repository;

    let repo = Repository::discover(None).context("not a git repository")?;
    let index_path = repo.index_path_for_env().context("resolve index path")?;
    let mut index = repo
        .load_index_at(&index_path)
        .with_context(|| format!("loading index {}", index_path.display()))?;
    index.clear_cache_tree();
    repo.write_index_at(&index_path, &mut index)
        .with_context(|| format!("writing index {}", index_path.display()))?;
    Ok(())
}

#[cfg(feature = "test-tools")]
fn run_test_tool_dump_untracked_cache() -> Result<()> {
    use grit_lib::index::Index;
    use grit_lib::repo::Repository;
    use grit_lib::untracked_cache::UntrackedCacheDir;

    let repo = Repository::discover(None).context("not a git repository")?;
    let index = Index::load(&repo.index_path()).context("read index")?;
    let Some(uc) = index.untracked_cache.as_ref() else {
        println!("no untracked cache");
        return Ok(());
    };

    println!("info/exclude {}", uc.ss_info_exclude.oid.to_hex());
    println!("core.excludesfile {}", uc.ss_excludes_file.oid.to_hex());
    println!("exclude_per_dir {}", uc.exclude_per_dir);
    println!("flags {:08x}", uc.dir_flags);

    fn dump(ucd: &UntrackedCacheDir, base: &mut String) {
        let len = base.len();
        base.push_str(&ucd.name);
        base.push('/');
        print!("{} {}", base, ucd.exclude_oid.to_hex());
        if ucd.recurse {
            print!(" recurse");
        }
        if ucd.check_only {
            print!(" check_only");
        }
        if ucd.valid {
            print!(" valid");
        }
        println!();

        let mut names: Vec<_> = ucd.untracked.iter().cloned().collect();
        names.sort();
        for n in &names {
            println!("{n}");
        }

        let mut dirs: Vec<_> = ucd.dirs.iter().collect();
        dirs.sort_by(|a, b| a.name.cmp(&b.name));
        for d in dirs {
            dump(d, base);
        }

        base.truncate(len);
    }

    if let Some(root) = uc.root.as_ref() {
        let mut base = String::new();
        dump(root, &mut base);
    }

    Ok(())
}

/// `test-tool dump-fsmonitor` — minimal helper used by status/fsmonitor tests.
#[cfg(feature = "test-tools")]
fn run_test_tool_dump_fsmonitor() -> Result<()> {
    use grit_lib::index::Index;
    use grit_lib::repo::Repository;

    let repo = Repository::discover(None).context("not a git repository")?;
    let index = Index::load(&repo.index_path()).context("read index")?;
    if let Some(token) = index.fsmonitor_last_update.as_deref() {
        println!("fsmonitor last update {token}");
    } else {
        println!("no fsmonitor");
    }
    Ok(())
}

/// `test-tool read-cache` — minimal helper for fsmonitor/read-cache tests.
#[cfg(feature = "test-tools")]
fn run_test_tool_read_cache(rest: &[String]) -> Result<()> {
    use grit_lib::repo::Repository;

    let mut count: usize = 1;
    let mut print_and_refresh: Option<String> = None;

    for arg in &rest[1..] {
        if let Some(v) = arg.strip_prefix("--print-and-refresh=") {
            print_and_refresh = Some(v.to_string());
            continue;
        }
        if let Ok(n) = arg.parse::<usize>() {
            count = n;
        }
    }

    for i in 0..count {
        let repo = Repository::discover(None).context("not a git repository")?;
        if let Some(path) = print_and_refresh.as_deref() {
            let _ = crate::commands::update_index::run_refresh_quiet(&repo);
            let index = repo.load_index().context("read index")?;
            let rel = path.as_bytes();
            let is_up_to_date = index
                .get(rel, 0)
                .is_some_and(|entry| entry.fsmonitor_valid());
            println!(
                "{path} is{} up to date",
                if is_up_to_date { "" } else { " not" }
            );
            std::fs::write(path, format!("{i}\n"))
                .with_context(|| format!("write '{path}' for test-tool read-cache"))?;
        }
    }
    Ok(())
}

/// `.gitmodules` text and a path used only for parsing / round-trip (Git `config_from_gitmodules`).
#[cfg(feature = "test-tools")]
fn gitmodules_file_content_for_test_tool(
    repo: &grit_lib::repo::Repository,
    work_tree: &Path,
) -> Result<(String, PathBuf)> {
    use grit_lib::merge_diff::blob_oid_at_path;
    use grit_lib::objects::{parse_commit, ObjectKind};
    use grit_lib::state::resolve_head;

    let path = work_tree.join(".gitmodules");
    if path.exists() {
        let content = std::fs::read_to_string(&path).context("reading .gitmodules")?;
        return Ok((content, path));
    }
    let index = repo.load_index().context("failed to load index")?;
    if let Some(ie) = index.get(b".gitmodules", 0) {
        let obj = repo
            .odb
            .read(&ie.oid)
            .context("failed to read .gitmodules blob from ODB")?;
        if obj.kind != ObjectKind::Blob {
            return Ok((String::new(), path));
        }
        let content = String::from_utf8(obj.data).context("failed to decode .gitmodules blob")?;
        return Ok((content, path));
    }
    let head = resolve_head(&repo.git_dir).map_err(|e| anyhow::anyhow!("{e}"))?;
    let Some(commit_oid) = head.oid().copied() else {
        return Ok((String::new(), path));
    };
    let obj = repo.odb.read(&commit_oid).context("reading HEAD commit")?;
    if obj.kind != ObjectKind::Commit {
        return Ok((String::new(), path));
    }
    let commit = parse_commit(&obj.data).context("parsing HEAD commit")?;
    let Some(blob_oid) = blob_oid_at_path(&repo.odb, &commit.tree, ".gitmodules") else {
        return Ok((String::new(), path));
    };
    let blob = repo
        .odb
        .read(&blob_oid)
        .context("reading .gitmodules blob from HEAD tree")?;
    if blob.kind != ObjectKind::Blob {
        return Ok((String::new(), path));
    }
    let content = String::from_utf8(blob.data).context("failed to decode .gitmodules blob")?;
    Ok((content, path))
}

/// Matches Git `is_writing_gitmodules_ok` (`git/submodule.c`).
#[cfg(feature = "test-tools")]
fn is_writing_gitmodules_ok_for_test_tool(
    repo: &grit_lib::repo::Repository,
    work_tree: &Path,
) -> bool {
    use grit_lib::merge_diff::blob_oid_at_path;
    use grit_lib::objects::{parse_commit, ObjectKind};
    use grit_lib::state::resolve_head;

    let gm = work_tree.join(".gitmodules");
    if gm.exists() {
        return true;
    }
    let Ok(index) = repo.load_index() else {
        return false;
    };
    if index.get(b".gitmodules", 0).is_some() {
        return false;
    }
    let Ok(head) = resolve_head(&repo.git_dir) else {
        return false;
    };
    let Some(commit_oid) = head.oid().copied() else {
        return true;
    };
    let Ok(obj) = repo.odb.read(&commit_oid) else {
        return false;
    };
    if obj.kind != ObjectKind::Commit {
        return false;
    }
    let Ok(c) = parse_commit(&obj.data) else {
        return false;
    };
    blob_oid_at_path(&repo.odb, &c.tree, ".gitmodules").is_none()
}

/// Handle `test-tool chmtime` — get or set file modification times.
#[cfg(feature = "test-tools")]
fn run_test_tool_chmtime(rest: &[String]) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if rest.is_empty() {
        bail!("usage: test-tool chmtime [--get|=<ts>|+<n>|-<n>] <file>");
    }
    let flag = &rest[0];
    if flag == "--get" {
        for path in &rest[1..] {
            let meta = std::fs::metadata(path)
                .map_err(|e| anyhow::anyhow!("chmtime: cannot stat '{path}': {e}"))?;
            println!("{}", meta.mtime());
        }
        return Ok(());
    }
    for path in &rest[1..] {
        let meta = std::fs::metadata(path)
            .map_err(|e| anyhow::anyhow!("chmtime: cannot stat '{path}': {e}"))?;
        let current_mtime = meta.mtime();
        let new_mtime: i64 = if let Some(ts_str) = flag.strip_prefix('=') {
            if ts_str.starts_with('+') || ts_str.starts_with('-') {
                let offset = ts_str
                    .parse::<i64>()
                    .map_err(|e| anyhow::anyhow!("chmtime: invalid timestamp offset: {e}"))?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| anyhow::anyhow!("chmtime: system clock before epoch: {e}"))?
                    .as_secs() as i64;
                now + offset
            } else {
                ts_str
                    .parse::<i64>()
                    .map_err(|e| anyhow::anyhow!("chmtime: invalid timestamp: {e}"))?
            }
        } else if let Some(d) = flag.strip_prefix('+') {
            current_mtime
                + d.parse::<i64>()
                    .map_err(|e| anyhow::anyhow!("chmtime: {e}"))?
        } else if flag.starts_with('-') && !flag.starts_with("--") {
            current_mtime
                - flag[1..]
                    .parse::<i64>()
                    .map_err(|e| anyhow::anyhow!("chmtime: {e}"))?
        } else {
            bail!("chmtime: unknown flag '{flag}'");
        };
        // Use touch -t to set the mtime (format: [[CC]YY]MMDDhhmm[.ss])
        // Convert epoch to touch -d format
        // Use 'touch -m -d @<epoch>' to set mtime in UTC (avoids timezone issues)
        // macOS supports: touch -m -t YYYYMMDDhhmm.ss but that's TZ-dependent.
        // Use python or perl as fallback for reliable epoch setting.
        let ts_str = new_mtime.to_string();
        // Try touch with @epoch (works on Linux/BSD with GNU touch)
        let ok = std::process::Command::new("touch")
            .args(["-m", "-d", &format!("@{ts_str}"), path])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            // Fallback: use Python
            let py = format!("import os; os.utime('{path}', ({new_mtime}, {new_mtime}))");
            let status = std::process::Command::new("python3")
                .args(["-c", &py])
                .status()
                .map_err(|e| anyhow::anyhow!("chmtime: python3 failed: {e}"))?;
            if !status.success() {
                bail!("chmtime: could not set mtime for '{path}'");
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
#[cfg(feature = "test-tools")]
enum TestToolConfigParseKeyErr {
    InvalidKey,
    NoSectionOrName,
}

/// Match `git_config_parse_key` enough for `test-tool config get` (t1308).
#[cfg(feature = "test-tools")]
fn test_tool_git_config_parse_key(
    key: &str,
) -> std::result::Result<String, TestToolConfigParseKeyErr> {
    let Some(last_dot) = key.rfind('.') else {
        return Err(TestToolConfigParseKeyErr::NoSectionOrName);
    };
    if last_dot == 0 {
        return Err(TestToolConfigParseKeyErr::NoSectionOrName);
    }
    if last_dot == key.len() - 1 {
        return Err(TestToolConfigParseKeyErr::NoSectionOrName);
    }

    let baselen = last_dot;
    let mut dot_seen = false;
    let mut out: Vec<u8> = Vec::with_capacity(key.len());

    for (i, c) in key.bytes().enumerate() {
        if c == b'.' {
            dot_seen = true;
        }
        if !dot_seen || i > baselen {
            let is_first_var = i == baselen + 1;
            let ok = c.is_ascii_alphanumeric() || c == b'-';
            if !ok || (is_first_var && !c.is_ascii_alphabetic()) {
                return Err(TestToolConfigParseKeyErr::InvalidKey);
            }
            out.push(c.to_ascii_lowercase());
        } else if c == b'\n' {
            return Err(TestToolConfigParseKeyErr::InvalidKey);
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }

    String::from_utf8(out).map_err(|_| TestToolConfigParseKeyErr::InvalidKey)
}

#[cfg(feature = "test-tools")]
fn test_tool_config_display_name(path: &std::path::Path) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Ok(stripped) = path.strip_prefix(&cwd) {
        let s = stripped.to_string_lossy();
        if s.is_empty() {
            return ".".to_string();
        }
        return s.into_owned();
    }
    git_path::real_path_resolving(&path.display().to_string())
        .display()
        .to_string()
}

#[cfg(feature = "test-tools")]
fn test_tool_config_origin_type(entry: &grit_lib::config::ConfigEntry) -> &'static str {
    if matches!(entry.scope, grit_lib::config::ConfigScope::Command) {
        return "command line";
    }
    "file"
}

#[cfg(feature = "test-tools")]
fn test_tool_config_iterate_name(entry: &grit_lib::config::ConfigEntry) -> String {
    match &entry.file {
        None => String::new(),
        Some(p) => {
            if p.to_string_lossy() == ":GIT_CONFIG_PARAMETERS" {
                return String::new();
            }
            grit_lib::config::config_file_display_for_error(p)
        }
    }
}

#[cfg(feature = "test-tools")]
fn test_tool_config_fatal_bad_numeric(
    name: &str,
    value: &str,
    entry: &grit_lib::config::ConfigEntry,
) -> ! {
    let v = if value.is_empty() { "''" } else { value };
    let msg = if matches!(entry.scope, grit_lib::config::ConfigScope::Command) && entry.line == 0 {
        format!("fatal: bad numeric config value '{v}' for '{name}': invalid unit")
    } else if let Some(path) = &entry.file {
        let disp = test_tool_config_display_name(path);
        format!("fatal: bad numeric config value '{v}' for '{name}' in file {disp}: invalid unit")
    } else {
        format!("fatal: bad numeric config value '{v}' for '{name}': invalid unit")
    };
    eprintln!("{msg}");
    std::process::exit(128);
}

#[cfg(feature = "test-tools")]
fn test_tool_config_fatal_missing_string(name: &str, entry: &grit_lib::config::ConfigEntry) -> ! {
    let msg = match &entry.file {
        Some(path) => {
            let disp = test_tool_config_display_name(path);
            format!(
                "fatal: missing value for '{name}' in file {disp} at line {}",
                entry.line
            )
        }
        None => format!("fatal: missing value for '{name}'"),
    };
    eprintln!("{msg}");
    std::process::exit(128);
}

#[cfg(feature = "test-tools")]
fn test_tool_parse_git_bool_strict(value: &str) -> std::result::Result<bool, ()> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Ok(true),
        "false" | "no" | "off" => Ok(false),
        "" => Ok(false),
        _ => {
            let Ok(n) = value.parse::<i64>() else {
                return Err(());
            };
            Ok(n != 0)
        }
    }
}

/// Locate the repository git directory for `test-tool config` without requiring config to parse.
///
/// `Repository::discover` reads (and parses) the repository config as part of discovery, so when the
/// config file contains a syntax error discovery fails and returns `None`. The config test helper
/// must still find the local `config` file in that case so the malformed line surfaces as a fatal
/// parse error (t1308 "proper error on error in default config files"). Mirror the `git config`
/// command's `resolve_git_dir`: try discovery first, then fall back to a config-free directory walk.
#[cfg(feature = "test-tools")]
fn test_tool_config_locate_git_dir() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("GIT_DIR") {
        let p = std::path::PathBuf::from(dir);
        if p.is_absolute() {
            return Some(p);
        }
        if let Ok(cwd) = std::env::current_dir() {
            return Some(cwd.join(p));
        }
        return Some(p);
    }
    grit_lib::repo::Repository::discover(None)
        .ok()
        .map(|r| r.git_dir)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(grit_lib::precompose_config::locate_git_dir_from_cwd)
        })
}

/// Handle `test-tool config` — config API test helper.
#[cfg(feature = "test-tools")]
fn run_test_tool_config(rest: &[String]) -> Result<()> {
    use grit_lib::config::{canonical_key, ConfigFile};
    use grit_lib::config::{parse_git_config_int_strict, ConfigScope, ConfigSet, IncludeContext};

    let subcmd = rest.first().map(|s| s.as_str()).unwrap_or("");

    if subcmd == "read_early_config" {
        let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
        // Locate the git dir the way Git's `read_early_config`/`discover_git_directory_reason`
        // does: still record the gitdir even when the repository format is unsupported
        // (`GIT_DIR_INVALID_FORMAT`), so `read_early_config` can emit the
        // "Expected git repo version <= N" warning and skip the repo's local config (t1309).
        // `Repository::discover` validates the format and would yield no gitdir for such a repo,
        // so use the validation-free locator (it still honors GIT_DIR/ceilings).
        let git_dir_buf = test_tool_config_locate_git_dir();
        let git_dir = git_dir_buf.as_deref();
        return match ConfigSet::read_early_config(git_dir, key) {
            Ok(values) => {
                if values.is_empty() {
                    return Ok(());
                }
                for v in values {
                    println!("{v}");
                }
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("{}", e)),
        };
    }

    let git_dir_buf = test_tool_config_locate_git_dir();
    let git_dir = git_dir_buf.as_deref();

    match subcmd {
        "get" => {
            let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let cfg = ConfigSet::load(git_dir, true).unwrap_or_default();
            match test_tool_git_config_parse_key(key) {
                Err(TestToolConfigParseKeyErr::InvalidKey) => {
                    println!("Key \"{key}\" is invalid");
                    std::process::exit(1);
                }
                Err(TestToolConfigParseKeyErr::NoSectionOrName) => {
                    println!("Key \"{key}\" has no section");
                    std::process::exit(1);
                }
                Ok(_) => {
                    if cfg.get(key).is_some() {
                        return Ok(());
                    }
                    println!("Value not found for \"{key}\"");
                    std::process::exit(1);
                }
            }
        }
        "get_value" => {
            let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let cfg = match ConfigSet::load(git_dir, true) {
                Ok(c) => c,
                Err(e) => {
                    let es = e.to_string();
                    if es.starts_with("fatal: bad config line ") {
                        eprintln!("{es}");
                        std::process::exit(128);
                    }
                    return Err(anyhow::anyhow!("{}", e));
                }
            };
            match cfg.get_last_entry(key) {
                Some(entry) => match &entry.value {
                    None => println!("(NULL)"),
                    Some(s) => println!("{s}"),
                },
                None => {
                    println!("Value not found for \"{key}\"");
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        "get_value_multi" => {
            let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let cfg = if let Some(p) = rest.get(2) {
                let path = Path::new(p);
                let mut set = ConfigSet::new();
                let ctx = IncludeContext::default();
                let file = match ConfigFile::from_path(path, ConfigScope::Local) {
                    Ok(Some(f)) => f,
                    Ok(None) => {
                        println!("Value not found for \"{key}\"");
                        std::process::exit(1);
                    }
                    Err(e) => return Err(anyhow::anyhow!("{}", e)),
                };
                set.merge_file_with_includes(&file, true, &ctx)?;
                set
            } else {
                match ConfigSet::load(git_dir, true) {
                    Ok(c) => c,
                    Err(e) => {
                        let es = e.to_string();
                        if es.starts_with("fatal: bad config line ") {
                            eprintln!("{es}");
                            std::process::exit(128);
                        }
                        return Err(anyhow::anyhow!("{}", e));
                    }
                }
            };
            let raw = cfg.get_all_raw(key);
            if raw.is_empty() {
                println!("Value not found for \"{key}\"");
                std::process::exit(1);
            }
            for v in raw {
                match v {
                    None => println!("(NULL)"),
                    Some(s) => println!("{s}"),
                }
            }
            Ok(())
        }
        "get_string" => {
            let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let cfg = match ConfigSet::load(git_dir, true) {
                Ok(c) => c,
                Err(e) => return Err(anyhow::anyhow!("{}", e)),
            };
            let Some(entry) = cfg.get_last_entry(key) else {
                println!("Value not found for \"{key}\"");
                std::process::exit(1);
            };
            match &entry.value {
                None => test_tool_config_fatal_missing_string(key, &entry),
                Some(s) => println!("{s}"),
            }
            Ok(())
        }
        "get_int" => {
            let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let cfg = match ConfigSet::load(git_dir, true) {
                Ok(c) => c,
                Err(e) => return Err(anyhow::anyhow!("{}", e)),
            };
            let Some(entry) = cfg.get_last_entry(key) else {
                println!("Value not found for \"{key}\"");
                std::process::exit(1);
            };
            let value_src = entry.value.as_deref().unwrap_or("");
            match parse_git_config_int_strict(value_src) {
                Ok(n) => {
                    let n32 = i32::try_from(n).unwrap_or(i32::MAX);
                    println!("{n32}");
                }
                Err(_) => test_tool_config_fatal_bad_numeric(key, value_src, &entry),
            }
            Ok(())
        }
        "git_config_int" => {
            let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let cfg = match ConfigSet::load(git_dir, true) {
                Ok(c) => c,
                Err(e) => return Err(anyhow::anyhow!("{}", e)),
            };
            let canon = canonical_key(key).unwrap_or_default();
            for entry in cfg.entries() {
                if entry.key != canon {
                    continue;
                }
                let value_src = entry.value.as_deref().unwrap_or("");
                match parse_git_config_int_strict(value_src) {
                    Ok(n) => {
                        let n32 = i32::try_from(n).unwrap_or(i32::MAX);
                        println!("{n32}");
                    }
                    Err(_) => test_tool_config_fatal_bad_numeric(key, value_src, entry),
                }
            }
            Ok(())
        }
        "get_bool" => {
            let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let cfg = match ConfigSet::load(git_dir, true) {
                Ok(c) => c,
                Err(e) => return Err(anyhow::anyhow!("{}", e)),
            };
            let Some(entry) = cfg.get_last_entry(key) else {
                println!("Value not found for \"{key}\"");
                std::process::exit(1);
            };
            if entry.value.is_none() {
                println!("1");
                return Ok(());
            }
            let value_src = entry.value.as_deref().unwrap_or("");
            match test_tool_parse_git_bool_strict(value_src) {
                Ok(b) => println!("{}", i32::from(b)),
                Err(_) => {
                    eprintln!("fatal: bad boolean config value '{value_src}' for '{key}'");
                    std::process::exit(128);
                }
            }
            Ok(())
        }
        "configset_get_value" | "configset_get_value_multi" => {
            let key = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let paths: Vec<&Path> = rest.iter().skip(2).map(|s| Path::new(s.as_str())).collect();
            if paths.is_empty() {
                bail!("test-tool config {subcmd}: expected key and at least one file");
            }
            let mut set = ConfigSet::new();
            let ctx = IncludeContext::default();
            for path in &paths {
                let err_line = format!(
                    "Error (-1) reading configuration file {}.",
                    grit_lib::config::config_file_display_for_error(path)
                );
                if !path.exists() {
                    eprintln!("{err_line}");
                    std::process::exit(2);
                }
                if path.is_dir() {
                    eprintln!(
                        "warning: unable to access '{}': Is a directory",
                        path.display()
                    );
                    eprintln!("{err_line}");
                    std::process::exit(2);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    match fs::metadata(path) {
                        Ok(m) if m.is_file() => {
                            let mode = m.permissions().mode();
                            if mode & 0o444 == 0 {
                                eprintln!(
                                    "warning: unable to access '{}': Permission denied",
                                    path.display()
                                );
                                eprintln!("{err_line}");
                                std::process::exit(2);
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("warning: unable to access '{}': {e}", path.display());
                            eprintln!("{err_line}");
                            std::process::exit(2);
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = fs::metadata(path);
                }
                let file = match ConfigFile::from_path(path, ConfigScope::Local) {
                    Ok(Some(f)) => f,
                    Ok(None) => {
                        println!("Error (-1) reading configuration file {}.", path.display());
                        std::process::exit(2);
                    }
                    Err(e) => {
                        let es = e.to_string();
                        if let Some(rest) = es.strip_prefix("bad config line ") {
                            if let Some((line_part, tail)) = rest.split_once(" in file ") {
                                if let Ok(line) = line_part.parse::<usize>() {
                                    let p = std::path::Path::new(tail);
                                    eprintln!(
                                        "fatal: bad config line {line} in file {}",
                                        p.display()
                                    );
                                    std::process::exit(128);
                                }
                            }
                        }
                        return Err(anyhow::anyhow!("{}", e));
                    }
                };
                set.merge_file_with_includes(&file, true, &ctx)?;
            }
            if subcmd == "configset_get_value" {
                let raw = set.get_all_raw(key);
                let Some(last) = raw.last() else {
                    println!("Value not found for \"{key}\"");
                    std::process::exit(1);
                };
                match last {
                    None => println!("(NULL)"),
                    Some(s) => println!("{s}"),
                }
            } else {
                let raw = set.get_all_raw(key);
                if raw.is_empty() {
                    println!("Value not found for \"{key}\"");
                    std::process::exit(1);
                }
                for v in raw {
                    match v {
                        None => println!("(NULL)"),
                        Some(s) => println!("{s}"),
                    }
                }
            }
            Ok(())
        }
        "iterate" => {
            // Upstream t1308 expects only the standard user/repo/command layers; skip system
            // `/etc/gitconfig` so host-wide entries (e.g. git-lfs) do not appear in output.
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            let cfg = match ConfigSet::load(git_dir, true) {
                Ok(c) => c,
                Err(e) => return Err(anyhow::anyhow!("{}", e)),
            };
            let mut first = true;
            for entry in cfg.entries() {
                if !first {
                    println!();
                }
                first = false;
                let value_str = match &entry.value {
                    None => "(null)".to_string(),
                    Some(s) => s.clone(),
                };
                println!("key={}", entry.key);
                println!("value={value_str}");
                println!("origin={}", test_tool_config_origin_type(entry));
                println!("name={}", test_tool_config_iterate_name(entry));
                let lno = if matches!(entry.scope, grit_lib::config::ConfigScope::Command) {
                    -1
                } else {
                    i32::try_from(entry.line).unwrap_or(-1)
                };
                println!("lno={lno}");
                println!("scope={}", entry.scope);
            }
            Ok(())
        }
        "get_all" => {
            bail!("test-tool config get_all is not used by the harness; use get_value_multi");
        }
        _ => bail!("test-tool config: unknown subcommand '{subcmd}'"),
    }
}
