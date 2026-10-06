//! `ext::` remote URLs (Git's `git-remote-ext` / connect helper).
//!
//! See [git-remote-ext](https://git-scm.com/docs/git-remote-ext).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use grit_lib::objects::ObjectId;

use crate::fetch_transport;
use crate::grit_exe::grit_executable;
use grit_lib::pkt_line;

/// Parsed `ext::<command> <args>...` URL (without the `ext::` prefix).
pub struct RemoteExtSpec {
    pub argv: Vec<String>,
    pub git_repo_path: Option<String>,
    pub git_vhost: Option<String>,
}

fn service_noprefix(service: &str) -> &str {
    service.strip_prefix("git-").unwrap_or(service)
}

/// Length of one `remote-ext` argument starting at `input` (matches `strip_escapes` scan in
/// `git/builtin/remote-ext.c`).
fn remote_ext_arg_byte_len(input: &str) -> Result<usize> {
    let bytes = input.as_bytes();
    let mut rpos = 0usize;
    let mut escape = false;
    while rpos < bytes.len() && (escape || bytes[rpos] != b' ') {
        if escape {
            let c = bytes[rpos] as char;
            match c {
                ' ' | '%' | 's' | 'S' => {}
                'G' | 'V' => {
                    if rpos != 1 {
                        bail!("remote-ext: '%{c}' must be first character of an argument");
                    }
                }
                _ => bail!("remote-ext: bad placeholder '%{c}'"),
            }
            escape = false;
        } else {
            escape = bytes[rpos] == b'%';
        }
        rpos += 1;
    }
    if escape {
        bail!("remote-ext: incomplete placeholder");
    }
    Ok(rpos)
}

/// Split `input` into the first argument and the remainder (skips one inter-arg space).
fn next_remote_ext_arg(input: &str) -> Result<(&str, &str)> {
    if input.is_empty() {
        return Ok(("", ""));
    }
    let len = remote_ext_arg_byte_len(input)?;
    let tok = &input[..len];
    let rest = input[len..].trim_start_matches(' ');
    Ok((tok, rest))
}

/// Expand placeholders in one remote-ext argument for `service`.
/// `%G` / `%V` arguments are returned as `Err` with the special kind and payload (Git does not pass
/// these argv entries to the child).
fn expand_one_remote_ext_arg(token: &str, service: &str) -> Result<Result<String, (char, String)>> {
    let service_np = service_noprefix(service);
    let arg_len = remote_ext_arg_byte_len(token)?;
    if arg_len != token.len() {
        bail!("remote-ext: trailing junk after argument");
    }
    let bytes = token.as_bytes();
    let special = if bytes.len() >= 2 && bytes[0] == b'%' {
        let c = bytes[1] as char;
        if c == 'G' || c == 'V' {
            Some(c)
        } else {
            None
        }
    } else {
        None
    };

    let skip = if special.is_some() { 2 } else { 0 };
    let mut out = String::new();
    let mut i = skip;
    let mut escape = false;
    while i < bytes.len() {
        if escape {
            let c = bytes[i] as char;
            match c {
                ' ' | '%' => out.push(c),
                's' => out.push_str(service_np),
                'S' => out.push_str(service),
                _ => bail!("remote-ext: bad placeholder '%{c}' in expansion"),
            }
            escape = false;
        } else if bytes[i] == b'%' {
            escape = true;
        } else {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    if escape {
        bail!("remote-ext: incomplete placeholder");
    }

    if let Some(sp) = special {
        return Ok(Err((sp, out)));
    }
    Ok(Ok(out))
}

/// Parse `ext::...` into argv and optional git:// request fields (`%G` / `%V`).
///
/// `service` is the git service the helper proxies (e.g. `git-upload-pack` for fetch/ls-remote,
/// `git-receive-pack` for push). It is substituted into `%s` / `%S` placeholders in the URL, so the
/// helper receives the correct command (matches `git/builtin/remote-ext.c`).
pub fn parse_remote_ext_url(url: &str, service: &str) -> Result<RemoteExtSpec> {
    let rest = url
        .strip_prefix("ext::")
        .with_context(|| format!("not an ext:: URL: {url}"))?;
    if rest.is_empty() {
        bail!("ext:: URL is empty");
    }

    let mut argv: Vec<String> = Vec::new();
    let mut git_repo: Option<String> = None;
    let mut git_vhost: Option<String> = None;
    let parse_service = service;

    let mut cursor = rest;
    while !cursor.is_empty() {
        let (tok, next) = next_remote_ext_arg(cursor)?;
        cursor = next;
        if tok.is_empty() {
            break;
        }
        match expand_one_remote_ext_arg(tok, parse_service)? {
            Ok(arg) => argv.push(arg),
            Err(('G', payload)) => git_repo = Some(payload),
            Err(('V', payload)) => git_vhost = Some(payload),
            Err((c, _)) => bail!("remote-ext: unknown special argument '%{c}'"),
        }
    }

    if argv.is_empty() {
        bail!("ext:: URL: no command");
    }
    Ok(RemoteExtSpec {
        argv,
        git_repo_path: git_repo,
        git_vhost,
    })
}

fn argv0_basename(argv0: &str) -> Option<&str> {
    Path::new(argv0).file_name()?.to_str()
}

/// When the URL is `sh -c '…git-upload-pack <args…>…'` (t5802), run `grit upload-pack <args…>` as
/// the child process instead of nesting shells. The inner script may prefix the command (e.g.
/// `echo … && git-upload-pack …`); match the upload-pack argv segment anywhere in the string.
fn resolve_ext_child_argv(parsed: &RemoteExtSpec) -> (PathBuf, Vec<String>) {
    if parsed.argv.len() == 3
        && argv0_basename(&parsed.argv[0]).is_some_and(|b| b == "sh" || b == "dash")
        && parsed.argv[1] == "-c"
    {
        let inner = parsed.argv[2].trim();
        if let Some(rest) = extract_git_upload_pack_args(inner) {
            let grit = grit_executable();
            let mut args = vec!["upload-pack".to_owned()];
            args.extend(rest.split_whitespace().map(|s| s.to_owned()));
            return (grit, args);
        }
    }
    (PathBuf::from(&parsed.argv[0]), parsed.argv[1..].to_vec())
}

/// Returns upload-pack arguments (after the service name) when `script` contains
/// `git-upload-pack` or `git upload-pack` as a command word.
fn extract_git_upload_pack_args(script: &str) -> Option<&str> {
    let bytes = script.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let is_word_start = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if !is_word_start {
            i += 1;
            continue;
        }
        let rest = &script[i..];
        let rest = if let Some(r) = rest.strip_prefix("git-upload-pack") {
            r
        } else if let Some(r) = rest.strip_prefix("git upload-pack") {
            r
        } else {
            i += 1;
            continue;
        };
        let rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let first = rest.as_bytes()[0];
        if first == b'&' || first == b'|' || first == b';' || first == b'>' || first == b'<' {
            i += 1;
            continue;
        }
        return Some(rest.trim());
    }
    None
}

/// When an `ext::` URL runs `grit upload-pack <dir>` (or equivalent), return the resolved on-disk
/// git directory so fetch can compute tag-following `want` lines against the real remote ODB.
pub fn try_resolve_ext_upload_pack_git_dir(ext_url: &str) -> Option<PathBuf> {
    let spec = parse_remote_ext_url(ext_url, "git-upload-pack").ok()?;
    let (prog, child_args) = resolve_ext_child_argv(&spec);
    let grit = grit_executable();
    if prog != grit || child_args.len() != 2 || child_args[0] != "upload-pack" {
        return None;
    }
    let mut repo = PathBuf::from(&child_args[1]);
    if repo.as_os_str() == "." {
        repo = std::env::current_dir()
            .and_then(|p| p.canonicalize())
            .unwrap_or(repo);
    } else if repo.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            repo = cwd.join(&repo);
        }
    }
    let git_dir = if repo.file_name().is_some_and(|n| n == ".git") {
        repo.clone()
    } else if repo.join(".git").is_dir() {
        repo.join(".git")
    } else {
        repo.clone()
    };
    fs::canonicalize(&git_dir).ok()
}

fn write_git_daemon_request(
    w: &mut impl Write,
    service: &str,
    repo_path: &str,
    vhost: Option<&str>,
) -> Result<()> {
    let mut inner: Vec<u8> = Vec::new();
    inner.extend_from_slice(service.as_bytes());
    inner.push(b' ');
    inner.extend_from_slice(repo_path.as_bytes());
    inner.push(0);
    if let Some(h) = vhost {
        inner.extend_from_slice(b"host=");
        inner.extend_from_slice(h.as_bytes());
        inner.push(0);
    }
    pkt_line::write_packet_raw(w, &inner).context("write ext:: git:// request")?;
    w.flush().ok();
    Ok(())
}

/// Fetch via `ext::` helper: spawn the user's command with stdin/stdout as the git wire, then run
/// the same upload-pack negotiation as local fetch.
///
/// `service` is typically `git-upload-pack` for fetch/clone.
#[allow(clippy::type_complexity)]
pub fn fetch_via_ext_skipping(
    local_git_dir: &Path,
    ext_url: &str,
    service: &str,
    refspecs: &[String],
    compute_wants: impl FnOnce(&[(String, ObjectId)]) -> anyhow::Result<Vec<ObjectId>>,
    filter_active: bool,
) -> Result<(
    Vec<(String, ObjectId)>,
    Vec<(String, ObjectId)>,
    Option<String>,
    Option<ObjectId>,
)> {
    let spec = parse_remote_ext_url(ext_url, service)?;
    let (prog, child_args) = resolve_ext_child_argv(&spec);
    let grit = grit_executable();
    let mut child = if prog == grit && child_args.len() == 2 && child_args[0] == "upload-pack" {
        let mut repo = PathBuf::from(&child_args[1]);
        if repo.as_os_str() == "." {
            repo = std::env::current_dir()
                .and_then(|p| p.canonicalize())
                .unwrap_or(repo);
        } else if repo.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                repo = cwd.join(&repo);
            }
        }
        fetch_transport::spawn_upload_pack_with_proto(None, &repo, 0).with_context(|| {
            format!(
                "failed to spawn upload-pack for ext:: (repo {})",
                repo.display()
            )
        })?
    } else {
        let mut cmd = Command::new(&prog);
        cmd.args(&child_args)
            .env("GIT_EXT_SERVICE", service)
            .env("GIT_EXT_SERVICE_NOPREFIX", service_noprefix(service))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        cmd.env_remove("GIT_TRACE_PACKET");
        cmd.env_remove("GIT_PROTOCOL");
        cmd.spawn().with_context(|| {
            format!(
                "failed to spawn ext:: command {} {:?}",
                prog.display(),
                child_args
            )
        })?
    };

    let mut stdin = child.stdin.take().context("ext:: stdin")?;
    let mut stdout = child.stdout.take().context("ext:: stdout")?;

    if let Some(ref repo_path) = spec.git_repo_path {
        write_git_daemon_request(&mut stdin, service, repo_path, spec.git_vhost.as_deref())?;
    }

    let (advertised, head_symref, saw_v1, saw_v2, server_sid) =
        fetch_transport::read_advertisement(&mut stdout)?;
    if saw_v2 {
        crate::trace2_transfer::emit_negotiated_version_client_fetch_v2();
    } else {
        crate::trace2_transfer::emit_negotiated_version_client_fetch(saw_v1);
    }
    if let Some(ref sid) = server_sid {
        crate::trace2_transfer::emit_server_sid(sid);
    }
    let wants = compute_wants(&advertised)?;
    if wants.is_empty() {
        if refspecs.is_empty() && advertised.is_empty() {
            drop(stdin);
            let _ = fetch_transport::drain_child_stdout_to_eof(&mut stdout);
            let status = child.wait()?;
            if !status.success() {
                bail!("ext:: helper exited with {}", status);
            }
            return Ok((Vec::new(), Vec::new(), head_symref, None));
        }
        if refspecs.is_empty() {
            drop(stdin);
            let _ = fetch_transport::drain_child_stdout_to_eof(&mut stdout);
            let status = child.wait()?;
            if !status.success() {
                bail!("ext:: helper exited with {}", status);
            }
            let remote_heads: Vec<_> = advertised
                .iter()
                .filter(|(n, _)| n.starts_with("refs/heads/"))
                .cloned()
                .collect();
            let remote_tags: Vec<_> = advertised
                .iter()
                .filter(|(n, _)| n.starts_with("refs/tags/"))
                .cloned()
                .collect();
            let head_advertised_oid = advertised
                .iter()
                .find(|(n, _)| n == "HEAD")
                .map(|(_, o)| *o);
            return Ok((remote_heads, remote_tags, head_symref, head_advertised_oid));
        }
        bail!("nothing to fetch (advertised {} ref(s))", advertised.len());
    }

    let remote_heads: Vec<_> = advertised
        .iter()
        .filter(|(n, _)| n.starts_with("refs/heads/"))
        .cloned()
        .collect();
    let remote_tags: Vec<_> = advertised
        .iter()
        .filter(|(n, _)| n.starts_with("refs/tags/"))
        .cloned()
        .collect();
    let head_advertised_oid = advertised
        .iter()
        .find(|(n, _)| n == "HEAD")
        .map(|(_, o)| *o);

    let pack_buf = fetch_transport::fetch_upload_pack_negotiate_pack_bytes_with_streams(
        local_git_dir,
        &advertised,
        &mut stdin,
        &mut stdout,
        &wants,
        None,
        None,
        None,
    )?;
    drop(stdin);

    let status = child.wait()?;
    if !status.success() {
        bail!("ext:: helper exited with {}", status);
    }

    if pack_buf.len() < 12 || &pack_buf[0..4] != b"PACK" {
        bail!("did not receive a pack file from ext:: transport");
    }

    fetch_transport::unpack_upload_pack_bytes(local_git_dir, &pack_buf, filter_active)?;

    Ok((remote_heads, remote_tags, head_symref, head_advertised_oid))
}

/// Spawn the `ext::` helper for a push, with stdin/stdout wired as the git wire protocol and the
/// service advertised as `git-receive-pack`. The returned child speaks the receive-pack protocol;
/// the caller drives the advertisement read and send-pack stream over its stdio.
///
/// When the helper resolves to `grit upload-pack <dir>` (the in-tree fast path used by other
/// `ext::` tests), it is rewritten to `grit receive-pack <dir>` so push works against a local
/// repository without an external program.
pub fn spawn_ext_receive_pack(ext_url: &str) -> Result<std::process::Child> {
    let service = "git-receive-pack";
    let spec = parse_remote_ext_url(ext_url, service)?;
    let (prog, child_args) = resolve_ext_child_argv(&spec);
    let grit = grit_executable();
    let mut child = if prog == grit && child_args.len() == 2 && child_args[0] == "upload-pack" {
        let mut repo = PathBuf::from(&child_args[1]);
        if repo.as_os_str() == "." {
            repo = std::env::current_dir()
                .and_then(|p| p.canonicalize())
                .unwrap_or(repo);
        } else if repo.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                repo = cwd.join(&repo);
            }
        }
        let mut cmd = Command::new(&grit);
        cmd.arg("receive-pack")
            .arg(&repo)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_TRACE_PACKET")
            .env_remove("GIT_PROTOCOL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        cmd.spawn().with_context(|| {
            format!(
                "failed to spawn receive-pack for ext:: (repo {})",
                repo.display()
            )
        })?
    } else {
        let mut cmd = Command::new(&prog);
        cmd.args(&child_args)
            .env("GIT_EXT_SERVICE", service)
            .env("GIT_EXT_SERVICE_NOPREFIX", service_noprefix(service))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        cmd.env_remove("GIT_TRACE_PACKET");
        cmd.env_remove("GIT_PROTOCOL");
        cmd.spawn().with_context(|| {
            format!(
                "failed to spawn ext:: command {} {:?}",
                prog.display(),
                child_args
            )
        })?
    };

    if let Some(ref repo_path) = spec.git_repo_path {
        let mut stdin = child.stdin.take().context("ext:: stdin")?;
        write_git_daemon_request(&mut stdin, service, repo_path, spec.git_vhost.as_deref())?;
        child.stdin = Some(stdin);
    }

    Ok(child)
}

/// Query refs from an `ext::` remote without fetching objects.
#[allow(clippy::type_complexity)]
pub fn ls_remote_via_ext(
    ext_url: &str,
    service: &str,
) -> Result<(Vec<(String, ObjectId)>, Option<String>)> {
    let spec = parse_remote_ext_url(ext_url, service)?;
    let (prog, child_args) = resolve_ext_child_argv(&spec);
    let grit = grit_executable();
    let mut child = if prog == grit && child_args.len() == 2 && child_args[0] == "upload-pack" {
        let mut repo = PathBuf::from(&child_args[1]);
        if repo.as_os_str() == "." {
            repo = std::env::current_dir()
                .and_then(|p| p.canonicalize())
                .unwrap_or(repo);
        } else if repo.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                repo = cwd.join(&repo);
            }
        }
        fetch_transport::spawn_upload_pack_with_proto(None, &repo, 0).with_context(|| {
            format!(
                "failed to spawn upload-pack for ext:: (repo {})",
                repo.display()
            )
        })?
    } else {
        let mut cmd = Command::new(&prog);
        cmd.args(&child_args)
            .env("GIT_EXT_SERVICE", service)
            .env("GIT_EXT_SERVICE_NOPREFIX", service_noprefix(service))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        cmd.env_remove("GIT_TRACE_PACKET");
        cmd.env_remove("GIT_PROTOCOL");
        cmd.spawn().with_context(|| {
            format!(
                "failed to spawn ext:: command {} {:?}",
                prog.display(),
                child_args
            )
        })?
    };

    let mut stdin = child.stdin.take().context("ext:: stdin")?;
    let mut stdout = child.stdout.take().context("ext:: stdout")?;

    if let Some(ref repo_path) = spec.git_repo_path {
        write_git_daemon_request(&mut stdin, service, repo_path, spec.git_vhost.as_deref())?;
    }

    let (advertised, head_symref, saw_v1, saw_v2, server_sid) =
        fetch_transport::read_advertisement(&mut stdout)?;
    if saw_v2 {
        crate::trace2_transfer::emit_negotiated_version_client_fetch_v2();
    } else {
        crate::trace2_transfer::emit_negotiated_version_client_fetch(saw_v1);
    }
    if let Some(ref sid) = server_sid {
        crate::trace2_transfer::emit_server_sid(sid);
    }

    drop(stdin);
    let _ = fetch_transport::drain_child_stdout_to_eof(&mut stdout);
    let status = child.wait()?;
    if !status.success() {
        bail!("ext:: helper exited with {}", status);
    }

    Ok((advertised, head_symref))
}
