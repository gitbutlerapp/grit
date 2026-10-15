//! Filesystem monitor integration for index stat refresh (`core.fsmonitor`).
//!
//! Queries Git's fsmonitor hook protocol v2 or the built-in daemon IPC socket
//! before worktree stat scans, updating `FSMN` token and `CE_FSMONITOR_VALID` bits
//! so unchanged paths can skip disk I/O (matching Git's `refresh_fsmonitor`).
//!
//! ```rust
//! use grit_lib::worktree::fsmonitor::FsmonitorQueryBoundary;
//! let _ = FsmonitorQueryBoundary(0);
//! ```

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

/// Nanoseconds since the Unix epoch, sampled immediately before a fsmonitor hook query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsmonitorQueryBoundary(pub u64);

/// Inputs for [`refresh_index`]; embedders supply the query boundary explicitly.
#[derive(Debug, Clone, Copy)]
pub struct FsmonitorRefreshOptions {
    /// Token Git stores after a hook protocol v1 query (sampled before spawning the hook).
    pub query_boundary: FsmonitorQueryBoundary,
}

impl FsmonitorRefreshOptions {
    /// Build refresh options from an explicit boundary (for tests and embedders).
    #[must_use]
    pub fn with_query_boundary(boundary: FsmonitorQueryBoundary) -> Self {
        Self {
            query_boundary: boundary,
        }
    }
}

/// Format the hook protocol v1 token string from a pre-sampled boundary.
#[must_use]
pub fn v1_hook_token(boundary: FsmonitorQueryBoundary) -> String {
    format!("{}", boundary.0)
}

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

/// Outcome of [`refresh_index`] for this index load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsmonitorRefreshResult {
    /// Token or valid bits changed (index should be persisted).
    pub index_changed: bool,
    /// When true, [`entry_skips_worktree_stat`] may skip I/O for valid entries.
    pub honor_valid_bits: bool,
}

impl FsmonitorRefreshResult {
    const UNCHANGED: Self = Self {
        index_changed: false,
        honor_valid_bits: false,
    };
}

/// Refresh fsmonitor validity on `index` when `core.fsmonitor` is enabled.
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
    options: &FsmonitorRefreshOptions,
) -> Result<FsmonitorRefreshResult> {
    if index.fsmonitor_refresh_ran {
        return Ok(FsmonitorRefreshResult::UNCHANGED);
    }
    index.fsmonitor_refresh_ran = true;

    let mode = fsmonitor_mode(config);
    if matches!(mode, FsmonitorMode::Disabled) {
        return Ok(FsmonitorRefreshResult::UNCHANGED);
    }

    let default_runner = system_command_runner();
    let runner = runner.unwrap_or(default_runner.as_ref());
    let token_before = index.fsmonitor_last_update.clone();
    let valid_before: u32 = index
        .entries()
        .iter()
        .filter(|e| e.fsmonitor_valid())
        .count() as u32;

    let hook_version = hook_version(config);
    let v1_query_token = if hook_version == 1 {
        Some(v1_hook_token(options.query_boundary))
    } else {
        None
    };

    // Git skips the hook until the index already has an FSMN token; seed the token and invalidate.
    if matches!(mode, FsmonitorMode::Hook(_)) && index.fsmonitor_last_update.is_none() {
        let token = v1_query_token
            .clone()
            .unwrap_or_else(|| bootstrap_token(options.query_boundary));
        index.set_fsmonitor_last_update(Some(token));
        invalidate_all_valid(index);
        index.fsmonitor_validity_changed();
        let token_after = index.fsmonitor_last_update.clone();
        let valid_after: u32 = index
            .entries()
            .iter()
            .filter(|e| e.fsmonitor_valid())
            .count() as u32;
        let changed = token_before != token_after || valid_before != valid_after;
        return Ok(FsmonitorRefreshResult {
            index_changed: changed,
            honor_valid_bits: false,
        });
    }

    let query = match mode {
        FsmonitorMode::Disabled => unreachable!("fsmonitor disabled above"),
        FsmonitorMode::Hook(path) => query_hook(
            runner,
            &path,
            work_tree,
            index,
            config,
            options.query_boundary,
            v1_query_token.clone(),
        ),
        FsmonitorMode::Ipc => query_ipc(git_dir, index),
    };

    let honor_valid_bits = match query {
        Ok(outcome) => {
            apply_query_result(index, config, outcome);
            true
        }
        Err(_) => {
            invalidate_all_valid(index);
            if index.fsmonitor_last_update.is_none() {
                index.set_fsmonitor_last_update(Some(
                    v1_query_token
                        .clone()
                        .unwrap_or_else(|| bootstrap_token(options.query_boundary)),
                ));
            }
            false
        }
    };

    let token_after = index.fsmonitor_last_update.clone();
    let valid_after: u32 = index
        .entries()
        .iter()
        .filter(|e| e.fsmonitor_valid())
        .count() as u32;
    Ok(FsmonitorRefreshResult {
        index_changed: token_before != token_after || valid_before != valid_after,
        honor_valid_bits,
    })
}

struct QueryOutcome {
    token: String,
    trivial: bool,
    changed_paths: Vec<String>,
}

fn bootstrap_token(boundary: FsmonitorQueryBoundary) -> String {
    format!("builtin:{}", boundary.0)
}

fn query_hook(
    runner: &dyn CommandRunner,
    hook: &Path,
    work_tree: &Path,
    index: &Index,
    config: &ConfigSet,
    _query_boundary: FsmonitorQueryBoundary,
    v1_query_token: Option<String>,
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
    if version == 1 {
        let token = v1_query_token.ok_or_else(|| {
            Error::Message("internal error: missing v1 fsmonitor query token".into())
        })?;
        return Ok(parse_v1_hook_response(&output.stdout, token));
    }
    parse_v2_hook_response(&output.stdout)
}

/// Parse hook v1 stdout: NUL-delimited changed paths only (no response token).
fn parse_v1_hook_response(buf: &[u8], token: String) -> QueryOutcome {
    let mut changed_paths = Vec::new();
    let mut trivial = false;
    for part in buf.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8_lossy(part);
        if path == "/" {
            trivial = true;
            break;
        }
        changed_paths.push(path.into_owned());
    }
    QueryOutcome {
        token,
        trivial,
        changed_paths,
    }
}

fn parse_v2_hook_response(buf: &[u8]) -> Result<QueryOutcome> {
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
    parse_v2_hook_response(&response)
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

/// Git `initialize_pipe_name` for fsmonitor IPC (testable on all platforms).
#[must_use]
pub fn git_style_windows_pipe_name_from_absolute_ipc(absolute_ipc_path: &str) -> String {
    let mut body = absolute_ipc_path.replace('\\', "/");
    if body.starts_with("//") {
        body = body[2..].to_string();
    }
    const PREFIX: &str = r"\\.\pipe\";
    let mut pipe = String::from(PREFIX);
    pipe.push_str(&body);
    let path_start = PREFIX.len();
    if body.len() >= 2 && body.as_bytes()[1] == b':' {
        pipe.replace_range(path_start + 1..path_start + 2, "_");
        let head = &pipe[..path_start + 2];
        let tail = pipe[path_start + 2..].replace('/', "\\");
        return format!("{head}{tail}");
    }
    pipe.replace('/', "\\")
}

#[cfg(windows)]
fn ipc_send_query_windows(ipc_path: &Path, message: &[u8]) -> Result<Vec<u8>> {
    use std::ffi::OsStr;
    use std::io::{BufReader, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GENERIC_READ, GENERIC_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

    let abs = std::path::absolute(ipc_path).map_err(Error::Io)?;
    let abs_git = crate::git_path::user_display_path(&abs);
    let pipe_name = git_style_windows_pipe_name_from_absolute_ipc(&abs_git);
    let mut pipe: Vec<u16> = OsStr::new(&pipe_name).encode_wide().collect();
    pipe.push(0);
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
        index.invalidate_fsmonitor_for_path(&path, ignorecase);
    }
}

fn invalidate_all_valid(index: &mut Index) {
    index.invalidate_all_fsmonitor_valid();
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
        let out = parse_v2_hook_response(raw).expect("parse");
        assert_eq!(out.token, "tok");
        assert!(!out.trivial);
        assert_eq!(out.changed_paths, vec!["foo.txt", "bar/"]);
    }

    #[test]
    fn parse_v2_trivial_response() {
        let raw = b"tok\0/\0";
        let out = parse_v2_hook_response(raw).expect("parse");
        assert!(out.trivial);
    }

    #[test]
    fn parse_v1_changed_paths() {
        let raw = b"tracked.txt\0";
        let out = parse_v1_hook_response(raw, v1_hook_token(FsmonitorQueryBoundary(123)));
        assert_eq!(out.token, "123");
        assert!(!out.trivial);
        assert_eq!(out.changed_paths, vec!["tracked.txt"]);
    }

    #[test]
    fn v1_hook_token_uses_injected_boundary() {
        assert_eq!(v1_hook_token(FsmonitorQueryBoundary(99_001)), "99001");
    }

    #[test]
    fn windows_pipe_name_local_drive_matches_git() {
        assert_eq!(
            git_style_windows_pipe_name_from_absolute_ipc(
                "C:/Users/foo/repo/.git/fsmonitor--daemon.ipc"
            ),
            r"\\.\pipe\C_\Users\foo\repo\.git\fsmonitor--daemon.ipc"
        );
    }

    #[test]
    fn windows_pipe_name_unc_skips_leading_slash() {
        assert_eq!(
            git_style_windows_pipe_name_from_absolute_ipc(
                "//server/share/repo/.git/fsmonitor--daemon.ipc"
            ),
            r"\\.\pipe\server\share\repo\.git\fsmonitor--daemon.ipc"
        );
    }

    #[test]
    fn parse_v1_trivial_response() {
        let raw = b"/\0";
        let out = parse_v1_hook_response(raw, v1_hook_token(FsmonitorQueryBoundary(456)));
        assert!(out.trivial);
        assert!(out.changed_paths.is_empty());
    }
}
