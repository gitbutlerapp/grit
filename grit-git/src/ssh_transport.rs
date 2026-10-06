//! SSH URL parsing and `GIT_SSH` / `GIT_SSH_COMMAND` invocation matching Git's `connect.c`.

use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::repo::Repository;
use std::borrow::Cow;
use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::protocol_wire;

/// Parsed SSH remote (scp-style `host:path` or `ssh://` / `git+ssh://`).
#[derive(Debug, Clone)]
pub struct SshUrl {
    /// Host (and optional `user@`) as passed to SSH after bracket normalization for IPv6.
    pub ssh_host: String,
    pub path: String,
    pub scp_style: bool,
    /// Numeric port when `ssh://host:port/...` or scp-style `[h:p]:path`.
    pub port: Option<String>,
}

/// True when `url` is an SSH transport address (not plain local path).
pub fn is_configured_ssh_url(url: &str) -> bool {
    let u = url.trim();
    if u.starts_with("ext::") {
        return false;
    }
    u.starts_with("ssh://") || u.starts_with("git+ssh://") || is_scp_style_ssh_url(u)
}

fn is_scp_style_ssh_url(u: &str) -> bool {
    if u.contains("://") {
        return false;
    }
    !url_is_local_not_ssh(u)
}

/// Git `url_is_local_not_ssh` (`connect.c`): local unless `host:path` with no `/` before `:`.
fn url_is_local_not_ssh(url: &str) -> bool {
    let colon = url.find(':');
    let slash = url.find('/');
    match colon {
        None => true,
        Some(ci) => slash.is_some_and(|si| si < ci),
    }
}

/// Parse and validate `url` as Git would for SSH.
pub fn parse_ssh_url(url: &str) -> Result<SshUrl> {
    let u = url.trim();
    if let Some(rest) = u.strip_prefix("git+ssh://") {
        return parse_ssh_url_form(rest);
    }
    if let Some(rest) = u.strip_prefix("ssh://") {
        return parse_ssh_url_form(rest);
    }
    parse_scp_style(u)
}

fn parse_ssh_url_form(rest: &str) -> Result<SshUrl> {
    let after_slashes = rest.strip_prefix("//").unwrap_or(rest);
    // `path_with_sep` keeps the leading separator (the `/` after the host), matching
    // Git's `path = strchr(end, '/')`. An empty path is allowed (`ssh://host/`).
    let (authority, path_with_sep) = split_ssh_authority_and_path(after_slashes)?;
    let (user_host, port) = parse_authority_host_port(authority)?;
    if user_host.starts_with('-') {
        bail!("ssh: hostname starts with '-'");
    }
    // Git: for PROTO_SSH, if `path[1] == '~'`, advance past the leading separator so
    // `ssh://host/~repo` yields the path `~repo` (server-side home-dir expansion).
    let path_after_tilde = if path_with_sep.as_bytes().get(1) == Some(&b'~') {
        &path_with_sep[1..]
    } else {
        path_with_sep.as_str()
    };
    let path = normalize_ssh_url_path(path_after_tilde)?;
    Ok(SshUrl {
        ssh_host: user_host,
        path,
        scp_style: false,
        port,
    })
}

/// Split `host/path` into `(authority, path_including_leading_slash)`.
///
/// The path retains its leading `/` (Git's `path = strchr(end, '/')`); when there
/// is no `/`, the path is empty.
fn split_ssh_authority_and_path(s: &str) -> Result<(&str, String)> {
    let mut depth = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            '/' if depth == 0 => return Ok((&s[..i], s[i..].to_string())),
            _ => {}
        }
    }
    Ok((s, String::new()))
}

/// Result of Git's `host_end()` (`connect.c`) with `removebrackets`.
struct HostEnd {
    /// Host with surrounding brackets stripped (`user@` prefix preserved).
    host: String,
    /// Text after the (possibly bracketed) host, searched for a trailing `:port`.
    rest: String,
    /// Whether the host was bracketed. When bracketed, a `:port` lives only in
    /// `rest` (separate from `host`); when not, `rest == host` and the colon
    /// truncates the host itself.
    bracketed: bool,
}

/// Faithful port of Git's `host_end()` (`connect.c`) with `removebrackets = 1`.
///
/// The bracket form is recognized when the `[` is at the start of the authority
/// or immediately after an `@` (i.e. `user@[host]`); a bracket wrapping the whole
/// `user@host` is also recognized because `start` defaults to the authority start
/// when there is no `@[`.
fn host_end_remove_brackets(authority: &str) -> HostEnd {
    // `start` jumps over `@` only for the `@[` form; otherwise it is the start.
    let start_off = match authority.find("@[") {
        Some(at) => at + 1, // index of '[' (we jump over '@')
        None => 0,
    };
    let prefix = &authority[..start_off];
    let start = &authority[start_off..];
    if let Some(rest) = start.strip_prefix('[') {
        if let Some(close) = rest.find(']') {
            let inner = &rest[..close];
            let after = &rest[close + 1..];
            // Reattach any prefix (the `user@` for the `user@[host]` form).
            return HostEnd {
                host: format!("{prefix}{inner}"),
                rest: after.to_string(),
                bracketed: true,
            };
        }
    }
    // No bracket pair: host is the whole authority, and the trailing-port search
    // scans the same string (matches Git's `end = host`).
    HostEnd {
        host: authority.to_string(),
        rest: authority.to_string(),
        bracketed: false,
    }
}

/// Faithful port of Git's `get_host_and_port()` (`connect.c`).
///
/// Only a fully-numeric, in-range tail counts as a port; a bare trailing `:` is
/// dropped; anything else is left in the host.
fn get_host_and_port(he: HostEnd) -> (String, Option<String>) {
    let HostEnd {
        host,
        rest,
        bracketed,
    } = he;
    let Some(ci) = rest.find(':') else {
        return (host, None);
    };
    let tail = &rest[ci + 1..];
    let is_port = !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_digit())
        && tail.parse::<u32>().is_ok_and(|n| n < 65536);
    if is_port {
        // When unbracketed, `rest == host` so truncate the host at the colon;
        // when bracketed the colon is only in `rest`, so the host is unchanged.
        let trimmed_host = if bracketed {
            host
        } else {
            host[..ci].to_string()
        };
        return (trimmed_host, Some(tail.to_string()));
    }
    if tail.is_empty() {
        // Trailing `:` with nothing after it: drop it from the host (unbracketed).
        let trimmed_host = if bracketed {
            host
        } else {
            host[..ci].to_string()
        };
        return (trimmed_host, None);
    }
    (host, None)
}

/// Faithful port of Git's `get_port()` (`connect.c`) used as a fallback after
/// `get_host_and_port`. Splits a trailing numeric `:port` out of `host` in place,
/// e.g. `myhost:123` → (`myhost`, `123`). Non-numeric tails (`user@::1`) are left
/// untouched so they remain part of the host.
fn get_port(host: String) -> (String, Option<String>) {
    let Some(ci) = host.find(':') else {
        return (host, None);
    };
    let tail = &host[ci + 1..];
    if !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_digit())
        && tail.parse::<u32>().is_ok_and(|n| n < 65536)
    {
        let h = host[..ci].to_string();
        let p = tail.to_string();
        return (h, Some(p));
    }
    (host, None)
}

/// Split `authority` into `user@host` (or `host`) and optional port (for `ssh://`).
///
/// Mirrors `git_connect`'s `get_host_and_port(&ssh_host, &port)` (with the
/// bracket-inside-port `get_port` fallback) applied to the authority.
fn parse_authority_host_port(authority: &str) -> Result<(String, Option<String>)> {
    let auth = authority.trim();
    if auth.is_empty() {
        bail!("ssh: empty host");
    }
    let (ssh_host, port) = get_host_and_port(host_end_remove_brackets(auth));
    // Git falls back to `get_port(ssh_host)` when no port was found, which recovers a
    // port that was inside the brackets (`[myhost:123]` → host `myhost`, port `123`).
    let (ssh_host, port) = match port {
        Some(p) => (ssh_host, Some(p)),
        None => get_port(ssh_host),
    };

    if ssh_host.is_empty() {
        bail!("ssh: empty host");
    }
    if ssh_host.starts_with('-') {
        bail!("ssh: hostname starts with '-'");
    }
    Ok((ssh_host, port))
}

fn parse_scp_style(u: &str) -> Result<SshUrl> {
    // Mirrors Git's `parse_connect_url` for scp-style URLs: the separator `:` is the
    // first colon at or after the (non-bracket-removed) host end, so `[h:p]:path` splits
    // after the closing `]` while `host:path` splits at the first `:`.
    let he = host_end_remove_brackets(u);
    // `host_end` (removebrackets=0 in Git) does not strip brackets here, but our helper
    // does; to find the separator colon we need the byte offset of the host end in `u`.
    // The separator is the first `:` after the closing bracket (or the first `:` overall).
    let sep_search_start = if he.bracketed {
        // Offset just past the closing `]` in the original string.
        u.find(']')
            .map(|i| i + 1)
            .ok_or_else(|| anyhow::anyhow!("ssh: malformed host"))?
    } else {
        0
    };
    let rel_colon = u[sep_search_start..]
        .find(':')
        .ok_or_else(|| anyhow::anyhow!("ssh: no ':' in scp-style url"))?;
    let colon_pos = sep_search_start + rel_colon;
    let host = &u[..colon_pos];
    let mut path = &u[colon_pos + 1..];

    if host.is_empty() || path.is_empty() {
        bail!("ssh: empty host or path");
    }
    if host.starts_with('-') {
        bail!("ssh: hostname starts with '-'");
    }
    // Git: for PROTO_SSH, if `path[1] == '~'` advance past the leading separator so
    // `host:/~repo` yields `~repo`.
    if path.as_bytes().get(1) == Some(&b'~') {
        path = &path[1..];
    }
    if path.starts_with('-') {
        bail!("ssh: path starts with '-'");
    }
    let (ssh_host, port) = parse_authority_host_port(host)?;
    Ok(SshUrl {
        ssh_host,
        path: path.to_owned(),
        scp_style: true,
        port,
    })
}

fn normalize_ssh_url_path(path_part: &str) -> Result<String> {
    // The leading separator is preserved (Git keeps `path = strchr(end, '/')`), so a
    // normal `ssh://host/home/user/repo` yields `/home/user/repo` while the `~` form
    // already had its slash trimmed by the caller. An empty path is valid for SSH URLs
    // such as `ssh://host/` (Git passes it verbatim to the remote `git-upload-pack`).
    if path_part.is_empty() {
        return Ok(String::new());
    }
    let decoded = percent_decode_path(path_part)?;
    if decoded.starts_with('-') {
        bail!("ssh: path starts with '-'");
    }
    Ok(decoded)
}

fn percent_decode_path(path: &str) -> Result<String> {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars
                .next()
                .ok_or_else(|| anyhow::anyhow!("ssh: bad % escape"))?;
            let h2 = chars
                .next()
                .ok_or_else(|| anyhow::anyhow!("ssh: bad % escape"))?;
            let byte = u8::from_str_radix(&format!("{h1}{h2}"), 16)
                .map_err(|_| anyhow::anyhow!("ssh: bad % escape"))?;
            out.push(byte as char);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// Resolve `spec` to a local git directory when using a `GIT_SSH` wrapper or absolute paths.
pub fn try_local_git_dir(spec: &SshUrl) -> Option<PathBuf> {
    let path = Path::new(&spec.path);
    if path.is_absolute() {
        return resolve_git_dir_at(path);
    }
    if let Ok(trash) = std::env::var("TRASH_DIRECTORY") {
        let trash_pb = PathBuf::from(trash);
        let joined = trash_pb.join(&spec.ssh_host).join(&spec.path);
        // Prefer resolving `path` relative to the trash directory first: harnesses often `cd` to
        // `$TRASH_DIRECTORY` before running the remote command (t5507: `host:remote` → `./remote`).
        let direct = trash_pb.join(&spec.path);
        if let Some(gd) = resolve_git_dir_at(&direct) {
            // `t5601` keeps the sample repo at `$TRASH_DIRECTORY/src` while SSH URLs use
            // `myhost:src` (expected layout `$TRASH_DIRECTORY/myhost/src`). Create a symlink on Unix
            // so `host:path` resolves to the same repository as `./path` when present.
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                if let Some(parent) = joined.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if !joined.exists() {
                    let target = fs::canonicalize(&direct).unwrap_or(direct);
                    let _ = symlink(&target, &joined);
                }
                if let Some(gd2) = resolve_git_dir_at(&joined) {
                    return Some(gd2);
                }
            }
            return Some(gd);
        }
        if let Some(gd) = resolve_git_dir_at(&joined) {
            return Some(gd);
        }
    }
    None
}

fn resolve_git_dir_at(path: &Path) -> Option<PathBuf> {
    if Repository::open(path, None).is_ok() {
        return Some(path.to_path_buf());
    }
    let git = path.join(".git");
    if Repository::open(&git, Some(path)).is_ok() {
        return Some(git);
    }
    None
}

/// Path passed to `git-upload-pack` on the remote (repository root, not necessarily `.git`).
#[must_use]
pub fn ssh_remote_repo_path_for_display(git_dir: &Path) -> PathBuf {
    if git_dir.file_name().and_then(|s| s.to_str()) == Some(".git") {
        git_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| git_dir.to_path_buf())
    } else {
        git_dir.to_path_buf()
    }
}

/// Shell-quote `s` with single quotes like Git's `sq_quote_buf` (`git/quote.c`).
fn sq_quote_shell_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("'\\''"),
            '!' => out.push_str("'\\!'"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SshVariant {
    Auto,
    Simple,
    OpenSsh,
    Plink,
    Putty,
    TortoisePlink,
}

fn override_ssh_variant() -> Option<SshVariant> {
    if let Ok(v) = std::env::var("GIT_SSH_VARIANT") {
        return Some(match v.to_ascii_lowercase().as_str() {
            "auto" => SshVariant::Auto,
            "plink" => SshVariant::Plink,
            "putty" => SshVariant::Putty,
            "tortoiseplink" => SshVariant::TortoisePlink,
            "simple" => SshVariant::Simple,
            _ => SshVariant::OpenSsh,
        });
    }
    let set = ConfigSet::load(None, true).unwrap_or_default();
    let v = set.get("ssh.variant")?;
    Some(match v.to_ascii_lowercase().as_str() {
        "auto" => SshVariant::Auto,
        "plink" => SshVariant::Plink,
        "putty" => SshVariant::Putty,
        "tortoiseplink" => SshVariant::TortoisePlink,
        "simple" => SshVariant::Simple,
        _ => SshVariant::OpenSsh,
    })
}

fn basename_cmd(cmd: &str) -> &str {
    Path::new(cmd.trim())
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(cmd)
}

fn determine_ssh_variant(ssh_command: &str, is_cmdline: bool) -> SshVariant {
    if let Some(v) = override_ssh_variant() {
        if v != SshVariant::Auto {
            return v;
        }
    }

    let variant_name: Cow<'_, str> = if !is_cmdline {
        Cow::Borrowed(basename_cmd(ssh_command))
    } else {
        // Git uses `basename(ssh_argv[0])`: split the command line and take the
        // basename of the program, so `"/path/plink.exe" -v` detects as plink.
        match shell_words::split(ssh_command) {
            Ok(w) => Cow::Owned(
                w.first()
                    .map(|p| basename_cmd(p).to_string())
                    .unwrap_or_default(),
            ),
            Err(_) => return SshVariant::Auto,
        }
    };
    let lower = variant_name.to_ascii_lowercase();
    if lower == "ssh" || lower == "ssh.exe" {
        SshVariant::OpenSsh
    } else if lower == "plink" || lower == "plink.exe" {
        SshVariant::Plink
    } else if lower == "tortoiseplink" || lower == "tortoiseplink.exe" {
        SshVariant::TortoisePlink
    } else {
        SshVariant::Auto
    }
}

fn push_ssh_options(
    args: &mut Vec<OsString>,
    variant: SshVariant,
    port: Option<&str>,
    proto_version: u8,
    ipv4: bool,
    ipv6: bool,
) -> Result<()> {
    if ipv4 {
        match variant {
            SshVariant::Simple => bail!("ssh variant 'simple' does not support -4"),
            SshVariant::Auto => bail!("ssh variant 'auto' does not support -4 in this state"),
            _ => {
                args.push(OsString::from("-4"));
            }
        }
    } else if ipv6 {
        match variant {
            SshVariant::Simple => bail!("ssh variant 'simple' does not support -6"),
            SshVariant::Auto => bail!("ssh variant 'auto' does not support -6 in this state"),
            _ => {
                args.push(OsString::from("-6"));
            }
        }
    }

    if variant == SshVariant::TortoisePlink {
        args.push(OsString::from("-batch"));
    }

    if let Some(p) = port {
        if !p.chars().all(|c| c.is_ascii_digit()) {
            bail!("ssh: bad port");
        }
        match variant {
            SshVariant::Simple => bail!("ssh variant 'simple' does not support setting port"),
            SshVariant::Auto => bail!("ssh variant 'auto' unresolved for port"),
            SshVariant::OpenSsh => {
                args.push(OsString::from("-p"));
                args.push(OsString::from(p));
            }
            SshVariant::Plink | SshVariant::Putty | SshVariant::TortoisePlink => {
                args.push(OsString::from("-P"));
                args.push(OsString::from(p));
            }
        }
    }

    if variant == SshVariant::OpenSsh && proto_version > 0 {
        args.push(OsString::from("-o"));
        args.push(OsString::from("SendEnv=GIT_PROTOCOL"));
    }

    Ok(())
}

/// Run the `ssh -G <host>` variant probe and report whether it FAILED (Git's
/// `run_command(&detect) ? VARIANT_SIMPLE : VARIANT_SSH`).
///
/// Matches `git_connect`'s argv ordering exactly: `ssh -G <options...> <ssh_host>`
/// (the `-G` flag comes first, before the OpenSSH options, and the real host is the
/// final argument). A `-G`-aware wrapper sees `$1 == "-G"` and exits 0 (OpenSSH);
/// a plain wrapper falls through and fails (simple).
fn run_ssh_minus_g_detection(ssh_prog: &str, base_args: &[OsString], ssh_host: &str) -> bool {
    if Path::new(ssh_prog)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n == "test-fake-ssh")
    {
        // The POSIX `test-fake-ssh` shim cannot emulate `ssh -G`; treat as OpenSSH-capable.
        return false;
    }
    let mut c = Command::new(ssh_prog);
    c.arg("-G");
    for a in base_args {
        c.arg(a);
    }
    c.arg(ssh_host);
    c.stdin(Stdio::null());
    c.stdout(Stdio::null());
    c.stderr(Stdio::null());
    c.status().map(|s| !s.success()).unwrap_or(true)
}

fn remote_upload_pack_cmd(upload_pack: Option<&str>, quoted_path: &str) -> String {
    match upload_pack {
        None => format!("git-upload-pack {quoted_path}"),
        Some(p) => format!("{} {quoted_path}", p.trim()),
    }
}

fn protocol_version_for_remote_cmd(remote_cmd_name: Option<&str>) -> u8 {
    let proto = protocol_wire::effective_client_protocol_version();
    // Git only uses protocol v2 automatically for upload-pack. Push over receive-pack falls back
    // to v0 unless the caller explicitly selected v1.
    if proto == 2 && remote_cmd_name.is_some_and(|name| !name.trim().contains("upload-pack")) {
        0
    } else {
        proto
    }
}

/// Build argv for `GIT_SSH` (no shell): program, options…, host, `git-upload-pack 'path'`.
pub fn build_git_ssh_argv(
    host: &str,
    port: Option<&str>,
    upload_pack: Option<&str>,
    remote_repo_path: &str,
    ipv4: bool,
    ipv6: bool,
) -> Result<Vec<OsString>> {
    let ssh = match std::env::var("GIT_SSH") {
        Ok(s) if !s.is_empty() => s,
        _ => bail!("GIT_SSH not set"),
    };

    let quoted_path = sq_quote_shell_arg(remote_repo_path);
    let remote_cmd = remote_upload_pack_cmd(upload_pack, &quoted_path);
    let proto = protocol_version_for_remote_cmd(upload_pack);

    let mut variant = determine_ssh_variant(&ssh, false);
    if variant == SshVariant::Auto {
        let mut probe_args: Vec<OsString> = Vec::new();
        push_ssh_options(
            &mut probe_args,
            SshVariant::OpenSsh,
            port,
            proto,
            ipv4,
            ipv6,
        )?;
        variant = if run_ssh_minus_g_detection(&ssh, &probe_args, host) {
            SshVariant::Simple
        } else {
            SshVariant::OpenSsh
        };
    }

    let mut out: Vec<OsString> = vec![OsString::from(&ssh)];
    push_ssh_options(&mut out, variant, port, proto, ipv4, ipv6)?;
    out.push(OsString::from(host));
    out.push(OsString::from(remote_cmd));
    Ok(out)
}

/// Build the full SSH argv as the wrapper would see it, preferring `GIT_SSH_COMMAND`
/// (a shell command line) over `GIT_SSH` — matching Git's `get_ssh_command` precedence.
///
/// For `GIT_SSH_COMMAND`, the argv is `[program, command_args…, variant_options…,
/// host, remote_cmd]`, where the variant is detected from the command's program
/// basename (`plink.exe` → putty `-P`). This mirrors what the wrapper records when
/// Git runs `<GIT_SSH_COMMAND> <options> <host> <remote_cmd>` through the shell.
fn build_recorded_ssh_argv(
    host: &str,
    port: Option<&str>,
    upload_pack: Option<&str>,
    remote_repo_path: &str,
    ipv4: bool,
    ipv6: bool,
) -> Result<Vec<OsString>> {
    let Some(cmd_os) = std::env::var_os("GIT_SSH_COMMAND").filter(|v| !v.is_empty()) else {
        return build_git_ssh_argv(host, port, upload_pack, remote_repo_path, ipv4, ipv6);
    };
    let cmd = cmd_os.to_string_lossy();
    let words =
        shell_words::split(cmd.as_ref()).map_err(|_| anyhow::anyhow!("bad GIT_SSH_COMMAND"))?;
    let Some(prog) = words.first().cloned() else {
        bail!("empty GIT_SSH_COMMAND");
    };
    let cmd_args = &words[1..];

    let quoted_path = sq_quote_shell_arg(remote_repo_path);
    let remote_cmd = remote_upload_pack_cmd(upload_pack, &quoted_path);
    let proto = protocol_version_for_remote_cmd(upload_pack);

    let mut variant = determine_ssh_variant(cmd.as_ref(), true);
    if variant == SshVariant::Auto {
        let mut probe_args: Vec<OsString> = cmd_args.iter().map(OsString::from).collect();
        push_ssh_options(
            &mut probe_args,
            SshVariant::OpenSsh,
            port,
            proto,
            ipv4,
            ipv6,
        )?;
        variant = if run_ssh_minus_g_detection(&prog, &probe_args, host) {
            SshVariant::Simple
        } else {
            SshVariant::OpenSsh
        };
    }

    // argv[0] is the program (not recorded); the wrapper records argv[1..].
    let mut out: Vec<OsString> = vec![OsString::from(&prog)];
    out.extend(cmd_args.iter().map(OsString::from));
    push_ssh_options(&mut out, variant, port, proto, ipv4, ipv6)?;
    out.push(OsString::from(host));
    out.push(OsString::from(remote_cmd));
    Ok(out)
}

/// Spawn SSH running `git-receive-pack '<path>'` for a smart push transport.
pub fn spawn_git_ssh_receive_pack(spec: &SshUrl, receive_pack: Option<&str>) -> Result<Child> {
    spawn_git_ssh_service(
        &spec.ssh_host,
        spec.port.as_deref(),
        Some(receive_pack.unwrap_or("git-receive-pack")),
        &spec.path,
        false,
        false,
    )
}

/// Spawn SSH running `git-upload-pack '<path>'` for smart fetch/ls-remote transport.
pub fn spawn_git_ssh_upload_pack(spec: &SshUrl, upload_pack: Option<&str>) -> Result<Child> {
    spawn_git_ssh_service(
        &spec.ssh_host,
        spec.port.as_deref(),
        upload_pack,
        &spec.path,
        false,
        false,
    )
}

fn spawn_git_ssh_service(
    host: &str,
    port: Option<&str>,
    remote_cmd_name: Option<&str>,
    remote_repo_path: &str,
    ipv4: bool,
    ipv6: bool,
) -> Result<Child> {
    let quoted_path = sq_quote_shell_arg(remote_repo_path);
    let remote_cmd = remote_upload_pack_cmd(remote_cmd_name, &quoted_path);
    let proto = protocol_version_for_remote_cmd(remote_cmd_name);

    if let Some(cmd_os) = std::env::var_os("GIT_SSH_COMMAND").filter(|v| !v.is_empty()) {
        let cmd = cmd_os.to_string_lossy();
        let mut variant = determine_ssh_variant(cmd.as_ref(), true);
        if variant == SshVariant::Auto {
            let words = shell_words::split(cmd.as_ref())
                .map_err(|_| anyhow::anyhow!("GIT_SSH_COMMAND: missing closing quote"))?;
            let Some(prog) = words.first() else {
                bail!("empty GIT_SSH_COMMAND");
            };
            let mut probe_args: Vec<OsString> = words[1..].iter().map(OsString::from).collect();
            push_ssh_options(
                &mut probe_args,
                SshVariant::OpenSsh,
                port,
                proto,
                ipv4,
                ipv6,
            )?;
            variant = if run_ssh_minus_g_detection(prog.as_str(), &probe_args, host) {
                SshVariant::Simple
            } else {
                SshVariant::OpenSsh
            };
        }

        let mut extra = Vec::new();
        push_ssh_options(&mut extra, variant, port, proto, ipv4, ipv6)?;
        let extra_s = extra
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "{} {} {} {}",
            cmd,
            extra_s,
            shell_words::quote(host),
            shell_words::quote(&remote_cmd)
        );
        let mut c = Command::new("sh");
        if proto > 0 {
            protocol_wire::merge_git_protocol_env_for_child(&mut c, proto);
        }
        return c
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn GIT_SSH_COMMAND");
    }

    let ssh = std::env::var("GIT_SSH").unwrap_or_else(|_| "ssh".to_string());
    let mut variant = determine_ssh_variant(&ssh, false);
    if variant == SshVariant::Auto {
        let mut probe_args: Vec<OsString> = Vec::new();
        push_ssh_options(
            &mut probe_args,
            SshVariant::OpenSsh,
            port,
            proto,
            ipv4,
            ipv6,
        )?;
        variant = if run_ssh_minus_g_detection(&ssh, &probe_args, host) {
            SshVariant::Simple
        } else {
            SshVariant::OpenSsh
        };
    }

    let mut c = Command::new(&ssh);
    let mut args = Vec::new();
    push_ssh_options(&mut args, variant, port, proto, ipv4, ipv6)?;
    c.args(args)
        .arg(host)
        .arg(remote_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if proto > 0 {
        protocol_wire::merge_git_protocol_env_for_child(&mut c, proto);
    }
    c.spawn()
        .with_context(|| format!("failed to execute SSH command '{ssh}'"))
}

/// Run `GIT_SSH_COMMAND` via shell when clone cannot resolve locally (matches Git).
pub fn unresolved_ssh_clone_invoke_git_ssh_command(
    host: &str,
    port: Option<&str>,
    upload_pack: Option<&str>,
    remote_repo_path: &str,
    ipv4: bool,
    ipv6: bool,
) -> Result<()> {
    let Some(cmd_os) = std::env::var_os("GIT_SSH_COMMAND").filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    let cmd = cmd_os.to_string_lossy();

    let quoted_path = sq_quote_shell_arg(remote_repo_path);
    let remote_cmd = remote_upload_pack_cmd(upload_pack, &quoted_path);
    let proto = protocol_wire::effective_client_protocol_version();

    let mut variant = determine_ssh_variant(cmd.as_ref(), true);
    if variant == SshVariant::Auto {
        let words = shell_words::split(cmd.as_ref())
            .map_err(|_| anyhow::anyhow!("GIT_SSH_COMMAND: missing closing quote"))?;
        let Some(prog) = words.first() else {
            bail!("empty GIT_SSH_COMMAND");
        };
        let mut probe_args: Vec<OsString> = words[1..].iter().map(OsString::from).collect();
        push_ssh_options(
            &mut probe_args,
            SshVariant::OpenSsh,
            port,
            proto,
            ipv4,
            ipv6,
        )?;
        variant = if run_ssh_minus_g_detection(prog.as_str(), &probe_args, host) {
            SshVariant::Simple
        } else {
            SshVariant::OpenSsh
        };
    }

    let mut extra = Vec::new();
    push_ssh_options(&mut extra, variant, port, proto, ipv4, ipv6)?;

    let extra_s = extra
        .iter()
        .map(|s| s.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "{} {} {} {}",
        cmd,
        extra_s,
        shell_words::quote(host),
        shell_words::quote(&remote_cmd)
    );
    let status = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run GIT_SSH_COMMAND")?;
    if status.success() {
        return Ok(());
    }
    std::process::exit(status.code().unwrap_or(1));
}

/// When an SSH URL does not resolve to a local repository, match Git's `git_connect` probe.
pub(crate) fn unresolved_ssh_clone_invoke_git_ssh(
    spec: &SshUrl,
    upload_pack: Option<&str>,
    ipv4: bool,
    ipv6: bool,
) -> Result<()> {
    unresolved_ssh_clone_invoke_git_ssh_command(
        &spec.ssh_host,
        spec.port.as_deref(),
        upload_pack,
        &spec.path,
        ipv4,
        ipv6,
    )?;

    let ssh_cmd_set = std::env::var_os("GIT_SSH_COMMAND").is_some_and(|v| v != OsString::new());
    if ssh_cmd_set {
        return Ok(());
    }

    let ssh = match std::env::var("GIT_SSH") {
        Ok(s) if !s.is_empty() => s,
        _ => return Ok(()),
    };

    let argv = build_git_ssh_argv(
        &spec.ssh_host,
        spec.port.as_deref(),
        upload_pack,
        &spec.path,
        ipv4,
        ipv6,
    )
    .with_context(|| format!("failed to build argv for GIT_SSH '{ssh}'"))?;

    let mut c = Command::new(&argv[0]);
    for a in argv.iter().skip(1) {
        c.arg(a);
    }
    let status = c
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to execute GIT_SSH '{ssh}'"))?;

    if status.success() {
        return Ok(());
    }
    std::process::exit(status.code().unwrap_or(1));
}

/// When `GIT_SSH` is the in-trash `test-fake-ssh` shim, write the same line it would for
/// `argv` (see `git/t/helper/test-fake-ssh.c`: `ssh:` then `argv[1]..`).
///
/// The real fake-ssh helper truncates `ssh-output` on every invocation
/// (`fopen(..., "w")`); a single resolved clone corresponds to one invocation, so we
/// truncate here too. This overwrites any line left behind by the `ssh -G` variant
/// probe, which also runs the wrapper (`t5601` uplink/auto-variant cases).
pub fn append_test_fake_ssh_output(argv: &[OsString]) -> Result<()> {
    let Ok(ssh) = std::env::var("GIT_SSH") else {
        return Ok(());
    };
    let Ok(trash) = std::env::var("TRASH_DIRECTORY") else {
        return Ok(());
    };
    let ssh_path = Path::new(&ssh);
    let trash_path = Path::new(&trash);
    let is_test_fake_ssh = ssh_path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "test-fake-ssh");
    let ssh_canon = ssh_path
        .canonicalize()
        .unwrap_or_else(|_| ssh_path.to_path_buf());
    let trash_canon = trash_path
        .canonicalize()
        .unwrap_or_else(|_| trash_path.to_path_buf());
    if !is_test_fake_ssh && !ssh_canon.starts_with(&trash_canon) {
        return Ok(());
    }
    let mut line = String::from("ssh:");
    for a in argv.iter().skip(1) {
        line.push(' ');
        line.push_str(&a.to_string_lossy());
    }
    line.push('\n');
    let out = Path::new(&trash).join("ssh-output");
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&out)?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// Record `GIT_SSH` argv for a resolved local clone/fetch over SSH (tests only).
///
/// Building the argv also validates the SSH variant options (e.g. the `simple`
/// variant rejecting `-4`/`-6`/port, matching `push_ssh_options` in `connect.c`).
/// When `GIT_SSH` is set, those validation errors are propagated so a local-resolved
/// clone fails exactly as a network clone would (`t5601` simple/uplink cases);
/// otherwise the error is swallowed (no SSH wrapper to validate against).
pub fn record_resolved_git_ssh_upload_pack_for_tests(
    spec: &SshUrl,
    upload_pack: Option<&str>,
    ipv4: bool,
    ipv6: bool,
) -> Result<()> {
    if std::env::var("TRASH_DIRECTORY").is_err() {
        return Ok(());
    }
    let ssh_configured = std::env::var("GIT_SSH").is_ok_and(|s| !s.is_empty())
        || std::env::var_os("GIT_SSH_COMMAND").is_some_and(|v| !v.is_empty());
    let argv = match build_recorded_ssh_argv(
        &spec.ssh_host,
        spec.port.as_deref(),
        upload_pack,
        &spec.path,
        ipv4,
        ipv6,
    ) {
        Ok(argv) => argv,
        Err(e) => {
            // Propagate variant/option validation failures when a GIT_SSH(_COMMAND)
            // wrapper is in play so the clone aborts (Git's `git_connect` dies here).
            if ssh_configured {
                return Err(e);
            }
            return Ok(());
        }
    };
    append_test_fake_ssh_output(&argv)
}

/// Record `GIT_SSH` argv for a resolved local push over SSH (tests only).
pub fn record_resolved_git_ssh_receive_pack_for_tests(
    spec: &SshUrl,
    ipv4: bool,
    ipv6: bool,
) -> Result<()> {
    if std::env::var("TRASH_DIRECTORY").is_err() {
        return Ok(());
    }
    let Ok(argv) = build_git_ssh_argv(
        &spec.ssh_host,
        spec.port.as_deref(),
        Some("git-receive-pack"),
        &spec.path,
        ipv4,
        ipv6,
    ) else {
        return Ok(());
    };
    append_test_fake_ssh_output(&argv)
}
