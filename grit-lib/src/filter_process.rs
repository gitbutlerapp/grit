//! Long-running Git filter protocol (`filter.<name>.process`), matching `git-filter` v2.
//!
//! See Git's `convert.c` (`apply_multi_file_filter`) and `sub-process.c` (handshake).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::{Arc, Mutex};

use crate::command_runner::{
    CommandEnvironment, CommandRunner, CommandSpec, CommandStdin, CommandStdio, ShellInvocation,
};
use crate::objects::ObjectId;
use crate::refs;
use crate::repo::Repository;

/// Max data bytes per pkt-line payload (Git `LARGE_PACKET_DATA_MAX`).
const LARGE_PACKET_DATA_MAX: usize = 65520 - 4;

const CAP_CLEAN: u32 = 1 << 0;
const CAP_SMUDGE: u32 = 1 << 1;
const CAP_DELAY: u32 = 1 << 2;

/// Optional metadata sent with smudge (ref, treeish, blob hex).
#[derive(Debug, Clone, Default)]
pub struct FilterSmudgeMeta {
    pub ref_name: Option<String>,
    pub treeish_hex: Option<String>,
    pub blob_hex: Option<String>,
}

/// Smudge metadata for path-only checkouts (`git checkout -- <paths>`): `blob=` only.
#[must_use]
pub fn smudge_meta_blob_only(blob_hex: &str) -> FilterSmudgeMeta {
    FilterSmudgeMeta {
        blob_hex: Some(blob_hex.to_string()),
        ..Default::default()
    }
}

/// Smudge metadata with `treeish=` only (e.g. `git reset --hard <commit>` / `git merge` checkout).
#[must_use]
pub fn smudge_meta_treeish_only(treeish_hex: &str, blob_hex: &str) -> FilterSmudgeMeta {
    FilterSmudgeMeta {
        treeish_hex: Some(treeish_hex.to_string()),
        blob_hex: Some(blob_hex.to_string()),
        ..Default::default()
    }
}

/// Process-smudge metadata for `git reset --hard <ref>` (t0021): `ref=` when the spec names a ref.
#[must_use]
pub fn smudge_meta_for_reset(
    repo: &Repository,
    commit_spec: &str,
    resolved_commit: &ObjectId,
    blob_hex: &str,
) -> FilterSmudgeMeta {
    let tip_hex = resolved_commit.to_string();
    let mut meta = FilterSmudgeMeta {
        treeish_hex: Some(tip_hex.clone()),
        blob_hex: Some(blob_hex.to_string()),
        ..Default::default()
    };
    let arg_lower = commit_spec.to_ascii_lowercase();
    let is_full_hex = arg_lower.len() == 40 && arg_lower.chars().all(|c| c.is_ascii_hexdigit());
    if is_full_hex && arg_lower == tip_hex.to_ascii_lowercase() {
        meta.ref_name = None;
        return meta;
    }
    let mut candidates: Vec<String> = Vec::new();
    if commit_spec == "HEAD" || commit_spec.starts_with("refs/") {
        candidates.push(commit_spec.to_string());
    } else {
        candidates.push(format!("refs/heads/{commit_spec}"));
        candidates.push(format!("refs/tags/{commit_spec}"));
        candidates.push(commit_spec.to_string());
    }
    for name in candidates {
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &name) {
            if oid == *resolved_commit {
                meta.ref_name = Some(name);
                break;
            }
        }
    }
    meta
}

pub fn smudge_meta_for_checkout(repo: &Repository, blob_hex: &str) -> FilterSmudgeMeta {
    let mut meta = FilterSmudgeMeta {
        blob_hex: Some(blob_hex.to_string()),
        ..Default::default()
    };
    let Ok(content) = std::fs::read_to_string(repo.git_dir.join("HEAD")) else {
        return meta;
    };
    let content = content.trim();
    if let Some(sym) = content.strip_prefix("ref: ") {
        let sym = sym.trim();
        meta.ref_name = Some(sym.to_string());
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, sym) {
            meta.treeish_hex = Some(oid.to_string());
        }
    } else if content.len() == 40 {
        if let Ok(oid) = ObjectId::from_hex(content) {
            meta.treeish_hex = Some(oid.to_string());
        }
    }
    meta
}

pub(crate) struct RunningFilter {
    #[allow(dead_code)]
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    caps: u32,
}

/// Per-repository filter-process registry (long-running `filter.*.process` drivers).
pub struct FilterProcessState {
    command_runner: Arc<dyn CommandRunner>,
    registry: Mutex<HashMap<String, Arc<Mutex<RunningFilter>>>>,
    disabled: Mutex<HashSet<String>>,
}

impl std::fmt::Debug for FilterProcessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterProcessState").finish_non_exhaustive()
    }
}

impl FilterProcessState {
    pub(crate) fn new(command_runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            command_runner,
            registry: Mutex::new(HashMap::new()),
            disabled: Mutex::new(HashSet::new()),
        }
    }

    pub(crate) fn command_runner(&self) -> &Arc<dyn CommandRunner> {
        &self.command_runner
    }

    pub(crate) fn shutdown_all(&self) {
        let entries = match self.registry.lock() {
            Ok(mut reg) => reg.drain().map(|(_, v)| v).collect::<Vec<_>>(),
            Err(_) => return,
        };
        for arc in entries {
            terminate_shared_filter(&arc);
        }
    }

    /// Stop using a process filter for the rest of this repository handle.
    pub fn disable_process_filter(&self, cmd: &str) {
        if let Ok(mut disabled) = self.disabled.lock() {
            disabled.insert(cmd.to_string());
        }
        self.remove_process_filter(cmd);
    }

    pub(crate) fn process_filter_is_disabled(&self, cmd: &str) -> bool {
        self.disabled
            .lock()
            .ok()
            .is_some_and(|disabled| disabled.contains(cmd))
    }

    pub(crate) fn remove_process_filter(&self, cmd: &str) {
        let arc = match self.registry.lock() {
            Ok(mut reg) => reg.remove(cmd),
            Err(_) => return,
        };
        if let Some(arc) = arc {
            terminate_shared_filter(&arc);
        }
    }

    pub(crate) fn registry(&self) -> &Mutex<HashMap<String, Arc<Mutex<RunningFilter>>>> {
        &self.registry
    }
}

/// Stop using a process filter for the rest of this repository handle.
pub fn disable_process_filter(state: &FilterProcessState, cmd: &str) {
    state.disable_process_filter(cmd);
}

fn terminate_shared_filter(arc: &Arc<Mutex<RunningFilter>>) {
    let Ok(mut rf) = arc.lock() else {
        return;
    };
    rf.stdin.take();
    rf.stdout.take();
    let _ = rf.child.kill();
    let _ = rf.child.wait();
}

fn process_transport_error(err: &str) -> bool {
    !err.starts_with("filter status:") && !err.starts_with("filter tail status:")
}

fn set_packet_header(len: usize, out: &mut [u8; 4]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out[0] = HEX[(len >> 12) & 0xf];
    out[1] = HEX[(len >> 8) & 0xf];
    out[2] = HEX[(len >> 4) & 0xf];
    out[3] = HEX[len & 0xf];
}

fn write_packet(stdin: &mut ChildStdin, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > LARGE_PACKET_DATA_MAX {
        return Err(std::io::Error::other("filter packet payload too large"));
    }
    let total = payload.len() + 4;
    let mut hdr = [0u8; 4];
    set_packet_header(total, &mut hdr);
    stdin.write_all(&hdr)?;
    stdin.write_all(payload)?;
    stdin.flush()?;
    Ok(())
}

fn write_packet_line(stdin: &mut ChildStdin, line: &str) -> std::io::Result<()> {
    let mut s = line.to_string();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    write_packet(stdin, s.as_bytes())
}

fn write_flush(stdin: &mut ChildStdin) -> std::io::Result<()> {
    stdin.write_all(b"0000")?;
    stdin.flush()
}

fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = r.read(&mut buf[off..])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "unexpected EOF reading pkt-line",
            ));
        }
        off += n;
    }
    Ok(())
}

fn read_packet_header(stdout: &mut ChildStdout) -> std::io::Result<Option<[u8; 4]>> {
    let mut hdr = [0u8; 4];
    let mut off = 0usize;
    while off < 4 {
        let n = stdout.read(&mut hdr[off..])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "unexpected EOF reading pkt-line",
            ));
        }
        off += n;
    }
    Ok(Some(hdr))
}

fn read_packet_payload(stdout: &mut ChildStdout) -> std::io::Result<Option<Vec<u8>>> {
    let Some(hdr) = read_packet_header(stdout)? else {
        return Ok(None);
    };
    let hex = std::str::from_utf8(&hdr)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let total = usize::from_str_radix(hex, 16).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid pkt-line header")
    })?;
    if total == 0 {
        return Ok(None);
    }
    if total < 4 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid pkt-line length",
        ));
    }
    let len = total - 4;
    let mut payload = vec![0u8; len];
    read_exact(stdout, &mut payload)?;
    Ok(Some(payload))
}

fn read_packet_line(stdout: &mut ChildStdout) -> std::io::Result<Option<String>> {
    let Some(payload) = read_packet_payload(stdout)? else {
        return Ok(None);
    };
    let s = String::from_utf8_lossy(&payload).into_owned();
    Ok(Some(s.trim_end_matches('\n').to_string()))
}

/// Read pkt-lines until flush; updates `acc` only when a `status=` line appears (matches Git
/// `subprocess_read_status` — if the segment is empty, `acc` is left unchanged).
fn read_status(stdout: &mut ChildStdout, acc: &mut String) -> std::io::Result<()> {
    while let Some(line) = read_packet_line(stdout)? {
        if let Some(rest) = line.strip_prefix("status=") {
            *acc = rest.to_string();
        }
    }
    Ok(())
}

fn read_packetized(stdout: &mut ChildStdout) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = read_packet_payload(stdout)? {
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn handshake(stdout: &mut ChildStdout, stdin: &mut ChildStdin) -> std::io::Result<u32> {
    // Match Git's test-tool rot13-filter: client sends only `version=2` before the first flush.
    write_packet_line(stdin, "git-filter-client")?;
    write_packet_line(stdin, "version=2")?;
    write_flush(stdin)?;

    // Match Git `sub-process.c` `handshake_version` error format
    // (`error("Unexpected line '%s', expected %s-server", ...)`), so callers can recognize a
    // non-filter subprocess (t0021 "invalid process filter must fail").
    let server = read_packet_line(stdout)?;
    let server_line = server.as_deref().unwrap_or("<flush packet>");
    if server_line != "git-filter-server" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Unexpected line '{server_line}', expected git-filter-server"),
        ));
    }
    let Some(ver_line) = read_packet_line(stdout)? else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Unexpected line '<flush packet>', expected version",
        ));
    };
    let ver = ver_line
        .strip_prefix("version=")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "expected version="))?;
    if ver != "2" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported filter protocol version {ver}"),
        ));
    }
    if read_packet_line(stdout)?.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected flush after version",
        ));
    }

    write_packet_line(stdin, "capability=clean")?;
    write_packet_line(stdin, "capability=smudge")?;
    write_packet_line(stdin, "capability=delay")?;
    write_flush(stdin)?;

    let mut caps = 0u32;
    while let Some(line) = read_packet_line(stdout)? {
        if let Some(name) = line.strip_prefix("capability=") {
            match name {
                "clean" => caps |= CAP_CLEAN,
                "smudge" => caps |= CAP_SMUDGE,
                "delay" => caps |= CAP_DELAY,
                _ => {}
            }
        }
    }

    Ok(caps)
}

fn spawn_running(runner: &dyn CommandRunner, cmd: &str) -> std::io::Result<RunningFilter> {
    let mut env = CommandEnvironment::inherit_process_only();
    env.remove.push("GIT_CONFIG_GLOBAL".into());
    let spec = CommandSpec {
        program: "sh".into(),
        args: Vec::new(),
        shell: Some(ShellInvocation::DashC {
            script: cmd.to_owned(),
            argv0: None,
            args: Vec::new(),
        }),
        cwd: None,
        env,
        stdin: CommandStdin::Pipe(Vec::new()),
        stdout: CommandStdio::Pipe,
        stderr: CommandStdio::Inherit,
    };
    let running = runner.spawn(&spec)?;
    let (child, stdin, stdout, _) = running
        .into_system_parts()
        .map_err(|_| std::io::Error::other("filter process is not backed by a live OS process"))?;
    let mut stdin = stdin.ok_or_else(|| std::io::Error::other("filter process missing stdin"))?;
    let mut stdout =
        stdout.ok_or_else(|| std::io::Error::other("filter process missing stdout"))?;

    let caps = handshake(&mut stdout, &mut stdin)?;

    Ok(RunningFilter {
        child,
        stdin: Some(stdin),
        stdout: Some(stdout),
        caps,
    })
}

/// Ensure the long-running filter for `cmd` is running (handshake complete).
pub fn ensure_process_filter_started(state: &FilterProcessState, cmd: &str) -> Result<(), String> {
    ensure_started(state, cmd)
}

fn ensure_started(state: &FilterProcessState, cmd: &str) -> Result<(), String> {
    let mut reg = state
        .registry()
        .lock()
        .map_err(|_| "filter registry poisoned".to_string())?;
    use std::collections::hash_map::Entry;
    match reg.entry(cmd.to_string()) {
        Entry::Occupied(_) => Ok(()),
        Entry::Vacant(v) => {
            let rf =
                spawn_running(state.command_runner.as_ref(), cmd).map_err(|e| e.to_string())?;
            v.insert(Arc::new(Mutex::new(rf)));
            Ok(())
        }
    }
}

fn write_packetized(stdin: &mut ChildStdin, data: &[u8]) -> std::io::Result<()> {
    let mut off = 0usize;
    while off < data.len() {
        let end = (off + LARGE_PACKET_DATA_MAX).min(data.len());
        write_packet(stdin, &data[off..end])?;
        off = end;
    }
    Ok(())
}

/// Run clean via long-running filter `cmd` for `path` and `input`.
pub fn apply_process_clean(
    state: &FilterProcessState,
    cmd: &str,
    path: &str,
    input: &[u8],
) -> Result<Vec<u8>, String> {
    if state.process_filter_is_disabled(cmd) {
        return Ok(input.to_vec());
    }
    ensure_started(state, cmd)?;
    let arc = {
        let reg = state
            .registry()
            .lock()
            .map_err(|_| "filter registry poisoned".to_string())?;
        reg.get(cmd)
            .cloned()
            .ok_or_else(|| "filter process not registered".to_string())?
    };
    let mut rf = arc
        .lock()
        .map_err(|_| "filter process mutex poisoned".to_string())?;
    if rf.caps & CAP_CLEAN == 0 {
        return Err("filter process does not support clean".to_string());
    }
    let mut stdin = rf
        .stdin
        .take()
        .ok_or_else(|| "filter stdin missing".to_string())?;
    let mut stdout = rf
        .stdout
        .take()
        .ok_or_else(|| "filter stdout missing".to_string())?;

    let result = (|| {
        write_packet_line(&mut stdin, "command=clean").map_err(|e| e.to_string())?;
        write_packet_line(&mut stdin, &format!("pathname={path}")).map_err(|e| e.to_string())?;
        write_flush(&mut stdin).map_err(|e| e.to_string())?;
        write_packetized(&mut stdin, input).map_err(|e| e.to_string())?;
        write_flush(&mut stdin).map_err(|e| e.to_string())?;

        let mut st = String::new();
        read_status(&mut stdout, &mut st).map_err(|e| e.to_string())?;
        if st != "success" {
            return Err(format!("filter status: {st}"));
        }
        let out = read_packetized(&mut stdout).map_err(|e| e.to_string())?;
        read_status(&mut stdout, &mut st).map_err(|e| e.to_string())?;
        if st != "success" {
            return Err(format!("filter tail status: {st}"));
        }
        Ok(out)
    })();

    rf.stdin = Some(stdin);
    rf.stdout = Some(stdout);
    result
}

/// One path deferred by a process filter that returned `status=delayed` (Git `delayed_checkout`).
#[derive(Debug, Clone)]
pub struct DelayedProcessCheckoutEntry {
    /// `filter.<name>.process` command line.
    pub filter_cmd: String,
    pub path: String,
    pub smudge_meta: FilterSmudgeMeta,
}

/// Paths waiting for `list_available_blobs` / retry smudge (Git `finish_delayed_checkout`).
#[derive(Debug, Default)]
pub struct DelayedProcessCheckout {
    pub entries: Vec<DelayedProcessCheckoutEntry>,
}

impl DelayedProcessCheckout {
    /// Record a delayed smudge; the file must be written after [`Self::finish`].
    pub fn push_delayed(
        &mut self,
        filter_cmd: String,
        path: String,
        smudge_meta: FilterSmudgeMeta,
    ) {
        self.entries.push(DelayedProcessCheckoutEntry {
            filter_cmd,
            path,
            smudge_meta,
        });
    }

    /// Complete delayed checkouts: query filters for available paths and materialize each file.
    ///
    /// Matches Git `finish_delayed_checkout` (entry.c): keep a list of the filters that delayed at
    /// least one path, and repeatedly ask each filter `list_available_blobs` until it returns an
    /// empty list (one final empty query per filter, which the t0021 log expects). A path the
    /// filter reports that we never delayed is the "is now available ... has not been delayed
    /// earlier" error (t0021 invalid file); any path still pending once every filter is done is the
    /// "was not filtered properly" error (t0021 missing file).
    ///
    /// Per-path problems are returned in [`DelayedCheckoutError::Reported`] so the caller can
    /// render Git's `error: ...` lines and exit non-zero. Git's
    /// `error("external filter '%s' ...")` quotes the filter command, and a buggy filter that
    /// offers an undelayed path is dropped immediately (it is not queried again).
    ///
    /// `convert_retry` matches Git `CE_RETRY`: empty blob through ident/encoding/eol then a
    /// second smudge without `can-delay` (filter returns cached output).
    pub fn finish(
        &mut self,
        filter_state: &FilterProcessState,
        mut convert_retry: impl FnMut(&str, &FilterSmudgeMeta) -> Result<Vec<u8>, String>,
        mut write_out: impl FnMut(&str, &[u8]) -> Result<(), String>,
    ) -> Result<(), DelayedCheckoutError> {
        // Active filters: every distinct filter command that delayed at least one path. Filters are
        // removed once they report no more available blobs (matching Git `dco->filters`).
        let mut active_cmds: Vec<String> = Vec::new();
        for e in &self.entries {
            if !active_cmds.contains(&e.filter_cmd) {
                active_cmds.push(e.filter_cmd.clone());
            }
        }

        let mut problems: Vec<DelayedCheckoutProblem> = Vec::new();

        while !active_cmds.is_empty() {
            let mut still_active: Vec<String> = Vec::new();
            for cmd in std::mem::take(&mut active_cmds) {
                let available = match list_available_blobs(filter_state, &cmd) {
                    Ok(paths) => paths,
                    Err(_) => {
                        // Filter reported an error: drop it and do not query it again.
                        continue;
                    }
                };
                if available.is_empty() {
                    // Filter is done; remove it from the active list.
                    continue;
                }
                let mut drop_filter = false;
                for path in available {
                    let Some(pos) = self
                        .entries
                        .iter()
                        .position(|e| e.filter_cmd == cmd && e.path == path)
                    else {
                        // The filter offered a path we never delayed (or already wrote). Match
                        // Git: record it and stop querying this (likely buggy) filter.
                        problems.push(DelayedCheckoutProblem::FilterSignaledAvailable {
                            filter_cmd: cmd.clone(),
                            path: path.clone(),
                        });
                        drop_filter = true;
                        continue;
                    };
                    let entry = self.entries.swap_remove(pos);
                    let data = convert_retry(&entry.path, &entry.smudge_meta)
                        .map_err(DelayedCheckoutError::Transport)?;
                    write_out(&entry.path, &data).map_err(DelayedCheckoutError::Transport)?;
                }
                // Keep querying this filter until it returns an empty list, unless it just sent us
                // an undelayed path (Git drops such a filter from the active list).
                if !drop_filter {
                    still_active.push(cmd);
                }
            }
            active_cmds = still_active;
        }

        // Any path the filters never made available was not filtered properly.
        for entry in &self.entries {
            problems.push(DelayedCheckoutProblem::PathNotFilteredProperly {
                path: entry.path.clone(),
            });
        }
        self.entries.clear();

        if !problems.is_empty() {
            return Err(DelayedCheckoutError::Reported(problems));
        }
        Ok(())
    }
}

/// One delayed-checkout outcome that would have been printed as Git's `error: ...` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelayedCheckoutProblem {
    /// Filter listed a path that was never delayed.
    FilterSignaledAvailable { filter_cmd: String, path: String },
    /// Filter never made a delayed path available.
    PathNotFilteredProperly { path: String },
}

/// Failure from [`DelayedProcessCheckout::finish`].
#[derive(Debug)]
pub enum DelayedCheckoutError {
    /// One or more per-path filter errors; the CLI should render them and exit non-zero.
    Reported(Vec<DelayedCheckoutProblem>),
    /// A transport/conversion error (not a per-path filter error) with a message to bubble up.
    Transport(String),
}

impl std::fmt::Display for DelayedCheckoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DelayedCheckoutError::Reported(problems) => {
                write!(f, "delayed checkout failed ({} problem(s))", problems.len())
            }
            DelayedCheckoutError::Transport(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for DelayedCheckoutError {}

/// True when `cmd` is running (or can be started) and advertises the `delay` capability.
pub fn process_filter_supports_delay(state: &FilterProcessState, cmd: &str) -> bool {
    if cmd.is_empty() {
        return false;
    }
    if state.process_filter_is_disabled(cmd) {
        return false;
    }
    if ensure_process_filter_started(state, cmd).is_err() {
        return false;
    }
    let Ok(reg) = state.registry().lock() else {
        return false;
    };
    let Some(arc) = reg.get(cmd) else {
        return false;
    };
    let Ok(rf) = arc.lock() else {
        return false;
    };
    (rf.caps & CAP_DELAY) != 0
}

fn list_available_blobs(state: &FilterProcessState, cmd: &str) -> Result<Vec<String>, String> {
    ensure_started(state, cmd)?;
    let arc = {
        let reg = state
            .registry()
            .lock()
            .map_err(|_| "filter registry poisoned".to_string())?;
        reg.get(cmd)
            .cloned()
            .ok_or_else(|| "filter process not registered".to_string())?
    };
    let mut rf = arc
        .lock()
        .map_err(|_| "filter process mutex poisoned".to_string())?;
    if rf.caps & CAP_DELAY == 0 {
        return Err("filter does not support delay".to_string());
    }
    let mut stdin = rf
        .stdin
        .take()
        .ok_or_else(|| "filter stdin missing".to_string())?;
    let mut stdout = rf
        .stdout
        .take()
        .ok_or_else(|| "filter stdout missing".to_string())?;

    let result = (|| {
        write_packet_line(&mut stdin, "command=list_available_blobs").map_err(|e| e.to_string())?;
        write_flush(&mut stdin).map_err(|e| e.to_string())?;
        let mut paths = Vec::new();
        loop {
            let line = read_packet_line(&mut stdout).map_err(|e| e.to_string())?;
            let Some(line) = line else {
                break;
            };
            if let Some(p) = line.strip_prefix("pathname=") {
                paths.push(p.to_string());
            }
        }
        let mut st = String::new();
        read_status(&mut stdout, &mut st).map_err(|e| e.to_string())?;
        if st != "success" {
            return Err(format!("list_available_blobs status: {st}"));
        }
        Ok(paths)
    })();

    rf.stdin = Some(stdin);
    rf.stdout = Some(stdout);
    result
}

/// Run smudge via long-running filter.
///
/// When `can_delay` is true and the filter returns `status=delayed`, returns `Ok(None)` after
/// recording is left to the caller ([`DelayedProcessCheckout`]).
pub fn apply_process_smudge(
    state: &FilterProcessState,
    cmd: &str,
    path: &str,
    input: &[u8],
    meta: Option<&FilterSmudgeMeta>,
    can_delay: bool,
) -> Result<Option<Vec<u8>>, String> {
    if state.process_filter_is_disabled(cmd) {
        return Ok(Some(input.to_vec()));
    }
    ensure_started(state, cmd)?;
    let arc = {
        let reg = state
            .registry()
            .lock()
            .map_err(|_| "filter registry poisoned".to_string())?;
        reg.get(cmd)
            .cloned()
            .ok_or_else(|| "filter process not registered".to_string())?
    };
    let mut rf = arc
        .lock()
        .map_err(|_| "filter process mutex poisoned".to_string())?;
    let caps = rf.caps;
    let mut stdin = rf
        .stdin
        .take()
        .ok_or_else(|| "filter stdin missing".to_string())?;
    let mut stdout = rf
        .stdout
        .take()
        .ok_or_else(|| "filter stdout missing".to_string())?;

    let result = (|| {
        if caps & CAP_SMUDGE == 0 {
            return Ok(Some(input.to_vec()));
        }
        write_packet_line(&mut stdin, "command=smudge").map_err(|e| e.to_string())?;
        write_packet_line(&mut stdin, &format!("pathname={path}")).map_err(|e| e.to_string())?;
        if let Some(m) = meta {
            if let Some(r) = &m.ref_name {
                write_packet_line(&mut stdin, &format!("ref={r}")).map_err(|e| e.to_string())?;
            }
            if let Some(t) = &m.treeish_hex {
                write_packet_line(&mut stdin, &format!("treeish={t}"))
                    .map_err(|e| e.to_string())?;
            }
            if let Some(b) = &m.blob_hex {
                write_packet_line(&mut stdin, &format!("blob={b}")).map_err(|e| e.to_string())?;
            }
        }
        if can_delay && (caps & CAP_DELAY) != 0 {
            write_packet_line(&mut stdin, "can-delay=1").map_err(|e| e.to_string())?;
        }
        write_flush(&mut stdin).map_err(|e| e.to_string())?;
        write_packetized(&mut stdin, input).map_err(|e| e.to_string())?;
        write_flush(&mut stdin).map_err(|e| e.to_string())?;

        let mut st = String::new();
        read_status(&mut stdout, &mut st).map_err(|e| e.to_string())?;
        if st == "delayed" {
            if !can_delay {
                return Err("unexpected delayed status from filter".to_string());
            }
            return Ok(None);
        }
        if st != "success" {
            return Err(format!("filter status: {st}"));
        }
        let out = read_packetized(&mut stdout).map_err(|e| e.to_string())?;
        read_status(&mut stdout, &mut st).map_err(|e| e.to_string())?;
        if st != "success" {
            return Err(format!("filter tail status: {st}"));
        }
        Ok(Some(out))
    })();

    if result
        .as_ref()
        .err()
        .is_some_and(|e| process_transport_error(e))
    {
        drop(stdin);
        drop(stdout);
        drop(rf);
        state.remove_process_filter(cmd);
        return result;
    }

    rf.stdin = Some(stdin);
    rf.stdout = Some(stdout);
    result
}
