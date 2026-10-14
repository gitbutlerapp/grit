//! Filesystem monitor integration for index stat refresh (`core.fsmonitor`).
//!
//! Queries Git's fsmonitor hook protocol v2 or the built-in daemon IPC socket
//! before worktree stat scans, updating `FSMN` token and `CE_FSMONITOR_VALID` bits
//! so unchanged paths can skip disk I/O (matching Git's `refresh_fsmonitor`).

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::command_runner::{
    system_command_runner, CommandEnvironment, CommandRunner, CommandSpec, CommandStdin,
    CommandStdio,
};
use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::index::Index;

/// Whether `core.fsmonitor` is configured for hook or built-in IPC mode.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FsmonitorMode {
    Disabled,
    Hook(PathBuf),
    Ipc,
}

fn fsmonitor_mode(config: &ConfigSet) -> FsmonitorMode {
    let Some(raw) = config.get("core.fsmonitor") else {
        return FsmonitorMode::Disabled;
    };
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("true") {
        return FsmonitorMode::Ipc;
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return FsmonitorMode::Disabled;
    }
    if trimmed.is_empty() {
        return FsmonitorMode::Disabled;
    }
    FsmonitorMode::Hook(PathBuf::from(trimmed))
}

fn hook_version(config: &ConfigSet) -> u32 {
    config
        .get("core.fsmonitorhookversion")
        .and_then(|v| v.trim().parse().ok())
        .filter(|&v| v == 1 || v == 2)
        .unwrap_or(2)
}

/// Refresh fsmonitor validity on `index` when `core.fsmonitor` is enabled.
///
/// Returns `true` when token or valid bits changed (index should be persisted).
///
/// # Errors
///
/// Returns I/O or hook failures only for unexpected spawn errors; a failed query
/// falls back to clearing all valid bits (Git-compatible) without error.
pub fn refresh_index(
    index: &mut Index,
    git_dir: &Path,
    work_tree: &Path,
    config: &ConfigSet,
    runner: Option<&dyn CommandRunner>,
) -> Result<bool> {
    let mode = fsmonitor_mode(config);
    if matches!(mode, FsmonitorMode::Disabled) {
        return Ok(false);
    }

    let default_runner = system_command_runner();
    let runner = runner.unwrap_or(default_runner.as_ref());
    let token_before = index.fsmonitor_last_update.clone();
    let valid_before: u32 = index
        .entries()
        .iter()
        .filter(|e| e.fsmonitor_valid())
        .count() as u32;

    let query = match mode {
        FsmonitorMode::Disabled => return Ok(false),
        FsmonitorMode::Hook(path) => query_hook(runner, &path, work_tree, index, config),
        FsmonitorMode::Ipc => query_ipc(git_dir, index),
    };

    match query {
        Ok(outcome) => {
            apply_query_result(index, config, outcome);
        }
        Err(_) => {
            invalidate_all_valid(index);
            if index.fsmonitor_last_update.is_none() {
                index.set_fsmonitor_last_update(Some(default_bootstrap_token()));
            }
        }
    }

    let token_after = index.fsmonitor_last_update.clone();
    let valid_after: u32 = index
        .entries()
        .iter()
        .filter(|e| e.fsmonitor_valid())
        .count() as u32;
    Ok(token_before != token_after || valid_before != valid_after)
}

struct QueryOutcome {
    token: String,
    trivial: bool,
    changed_paths: Vec<String>,
}

fn default_bootstrap_token() -> String {
    format!("builtin:{}", filetime::FileTime::now().unix_seconds())
}

fn query_hook(
    runner: &dyn CommandRunner,
    hook: &Path,
    work_tree: &Path,
    index: &Index,
    config: &ConfigSet,
) -> Result<QueryOutcome> {
    let version = hook_version(config);
    let last = index.fsmonitor_last_update.as_deref().unwrap_or("");
    let spec = CommandSpec {
        program: hook.to_path_buf(),
        args: vec![OsString::from(version.to_string()), OsString::from(last)],
        cwd: Some(work_tree.to_path_buf()),
        env: CommandEnvironment::inherit_process_only(),
        stdin: CommandStdin::Null,
        stdout: CommandStdio::Pipe,
        stderr: CommandStdio::Null,
        shell: None,
    };
    let child = runner.spawn(&spec).map_err(Error::Io)?;
    let output = child.wait_with_output().map_err(Error::Io)?;
    if !output.status.success {
        return Err(Error::Message(format!(
            "fsmonitor hook exited with {}",
            output.status.code.unwrap_or(-1)
        )));
    }
    parse_nul_response(&output.stdout, version == 1)
}

fn parse_nul_response(buf: &[u8], v1: bool) -> Result<QueryOutcome> {
    if v1 {
        let trivial = buf.first() == Some(&b'/');
        return Ok(QueryOutcome {
            token: default_bootstrap_token(),
            trivial,
            changed_paths: Vec::new(),
        });
    }
    let mut parts = buf.split(|&b| b == 0).filter(|p| !p.is_empty());
    let token = parts
        .next()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Error::Message("fsmonitor hook returned empty token".into()))?;
    let mut changed_paths = Vec::new();
    let mut trivial = false;
    for p in parts {
        let s = String::from_utf8_lossy(p);
        if s == "/" {
            trivial = true;
            break;
        }
        changed_paths.push(s.into_owned());
    }
    Ok(QueryOutcome {
        token,
        trivial,
        changed_paths,
    })
}

fn query_ipc(git_dir: &Path, index: &Index) -> Result<QueryOutcome> {
    let ipc_path = git_dir.join("fsmonitor--daemon.ipc");
    let token = index
        .fsmonitor_last_update
        .as_deref()
        .unwrap_or("builtin:fake");
    let response = ipc_send_query(&ipc_path, token.as_bytes())?;
    parse_nul_response(&response, false)
}

fn ipc_send_query(ipc_path: &Path, message: &[u8]) -> Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::io::BufReader;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let stream = UnixStream::connect(ipc_path).map_err(Error::Io)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(Error::Io)?;
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .map_err(Error::Io)?;
        let mut stream = stream;
        crate::pkt_line::write_packet_raw(&mut stream, message).map_err(Error::Io)?;
        crate::pkt_line::write_flush(&mut stream).map_err(Error::Io)?;
        stream.flush().map_err(Error::Io)?;

        let mut reader = BufReader::new(stream);
        let mut out = Vec::new();
        while let Some(pkt) = crate::pkt_line::read_packet(&mut reader).map_err(Error::Io)? {
            match pkt {
                crate::pkt_line::Packet::Data(s) => out.extend_from_slice(s.as_bytes()),
                crate::pkt_line::Packet::Flush => break,
                crate::pkt_line::Packet::Delim | crate::pkt_line::Packet::ResponseEnd => break,
            }
        }
        Ok(out)
    }
    #[cfg(windows)]
    {
        ipc_send_query_windows(ipc_path, message)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (ipc_path, message);
        Err(Error::Message(
            "fsmonitor IPC is not supported on this platform".into(),
        ))
    }
}

#[cfg(windows)]
fn ipc_send_query_windows(ipc_path: &Path, message: &[u8]) -> Result<Vec<u8>> {
    use std::ffi::OsStr;
    use std::io::{BufReader, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GENERIC_READ, GENERIC_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

    fn path_to_pipe_name(path: &Path) -> Vec<u16> {
        let mut wide: Vec<u16> = OsStr::new("\\\\.\\pipe\\").encode_wide().collect();
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut rest = canonical.as_os_str().encode_wide().collect::<Vec<_>>();
        if rest.len() >= 2 && rest[1] == b':' as u16 {
            rest[1] = b'_' as u16;
        }
        for ch in &mut rest {
            if *ch == b'/' as u16 {
                *ch = b'\\' as u16;
            }
        }
        wide.extend(rest);
        wide.push(0);
        wide
    }

    let pipe = path_to_pipe_name(ipc_path);
    unsafe {
        if WaitNamedPipeW(pipe.as_ptr(), 3000) == 0 {
            return Err(Error::Message("fsmonitor daemon pipe not available".into()));
        }
        let handle = CreateFileW(
            pipe.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(Error::Message("fsmonitor IPC connect failed".into()));
        }
        let mut file = std::fs::File::from_raw_handle(handle as _);
        crate::pkt_line::write_packet_raw(&mut file, message).map_err(Error::Io)?;
        crate::pkt_line::write_flush(&mut file).map_err(Error::Io)?;
        file.flush().map_err(Error::Io)?;
        let _ = FlushFileBuffers(handle);

        let mut reader = BufReader::new(file);
        let mut out = Vec::new();
        while let Some(pkt) = crate::pkt_line::read_packet(&mut reader).map_err(Error::Io)? {
            match pkt {
                crate::pkt_line::Packet::Data(s) => out.extend_from_slice(s.as_bytes()),
                crate::pkt_line::Packet::Flush => break,
                crate::pkt_line::Packet::Delim | crate::pkt_line::Packet::ResponseEnd => break,
            }
        }
        Ok(out)
    }
}

fn apply_query_result(index: &mut Index, config: &ConfigSet, outcome: QueryOutcome) {
    index.set_fsmonitor_last_update(Some(outcome.token));
    if outcome.trivial {
        invalidate_all_valid(index);
        return;
    }
    if outcome.changed_paths.is_empty() {
        return;
    }
    let ignorecase = config
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    for path in outcome.changed_paths {
        invalidate_path(index, &path, ignorecase);
    }
    index.fsmonitor_validity_changed();
}

fn invalidate_all_valid(index: &mut Index) {
    for entry in index.entries_mut() {
        entry.set_fsmonitor_valid(false);
    }
    index.fsmonitor_validity_changed();
}

fn invalidate_path(index: &mut Index, path: &str, ignorecase: bool) {
    let is_dir = path.ends_with('/');
    let prefix = path.trim_end_matches('/');
    if prefix.is_empty() {
        invalidate_all_valid(index);
        return;
    }
    for entry in index.entries_mut() {
        if entry.stage() != 0 {
            continue;
        }
        let Some(rel) = std::str::from_utf8(&entry.path).ok() else {
            continue;
        };
        let matches = if is_dir {
            rel == prefix || rel.starts_with(&format!("{prefix}/"))
        } else if ignorecase {
            rel.eq_ignore_ascii_case(prefix)
        } else {
            rel == prefix
        };
        if matches {
            entry.set_fsmonitor_valid(false);
        }
    }
}

/// When true, this index entry can skip worktree stat I/O during diff/status.
#[must_use]
pub fn entry_skips_worktree_stat(entry: &crate::index::IndexEntry, index: &Index) -> bool {
    index.fsmonitor_last_update.is_some() && entry.fsmonitor_valid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_v2_nul_response_token_and_paths() {
        let raw = b"tok\0foo.txt\0bar/\0";
        let out = parse_nul_response(raw, false).expect("parse");
        assert_eq!(out.token, "tok");
        assert!(!out.trivial);
        assert_eq!(out.changed_paths, vec!["foo.txt", "bar/"]);
    }

    #[test]
    fn parse_v2_trivial_response() {
        let raw = b"tok\0/\0";
        let out = parse_nul_response(raw, false).expect("parse");
        assert!(out.trivial);
    }
}
