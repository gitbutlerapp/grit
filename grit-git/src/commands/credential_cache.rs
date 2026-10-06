//! `grit credential-cache` — cache credentials in memory.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

/// Arguments for `grit credential-cache`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The credential-helper action: get, store, or erase.
    pub action: String,

    /// Timeout in seconds for cached credentials (default: 900).
    #[arg(long, default_value_t = 900)]
    pub timeout: u64,

    /// Path to the cache daemon socket.
    #[arg(long)]
    pub socket: Option<String>,
}

#[derive(Clone, Debug)]
struct CacheEntry {
    fields: BTreeMap<String, String>,
    expires_at: Instant,
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at || password_expired(&self.fields)
    }

    fn matches_get(&self, query: &BTreeMap<String, String>) -> bool {
        matches_field(&self.fields, query, "protocol")
            && matches_field(&self.fields, query, "host")
            && matches_field(&self.fields, query, "username")
            && matches_field(&self.fields, query, "path")
    }

    fn matches_erase(&self, query: &BTreeMap<String, String>) -> bool {
        for (key, value) in query {
            if key == "capability[]" || key == "wwwauth[]" || key == "state[]" {
                continue;
            }
            if self.fields.get(key) != Some(value) {
                return false;
            }
        }
        true
    }

    fn matches_store_identity(&self, query: &BTreeMap<String, String>) -> bool {
        matches_field(&self.fields, query, "protocol")
            && matches_field(&self.fields, query, "host")
            && matches_exact_if_present(&self.fields, query, "username")
            && matches_exact_if_present(&self.fields, query, "path")
    }

    fn usable_for_query(&self, query: &BTreeMap<String, String>) -> bool {
        if (self.fields.contains_key("authtype") || self.fields.contains_key("credential"))
            && query
                .get("capability[]")
                .is_none_or(|value| value != "authtype")
        {
            return false;
        }
        true
    }
}

fn matches_field(
    stored: &BTreeMap<String, String>,
    query: &BTreeMap<String, String>,
    key: &str,
) -> bool {
    query
        .get(key)
        .filter(|value| !value.is_empty())
        .is_none_or(|value| stored.get(key) == Some(value))
}

fn matches_exact_if_present(
    stored: &BTreeMap<String, String>,
    query: &BTreeMap<String, String>,
    key: &str,
) -> bool {
    query
        .get(key)
        .is_none_or(|value| stored.get(key) == Some(value))
}

fn read_credential_from_stdin() -> Result<BTreeMap<String, String>> {
    let stdin = io::stdin();
    read_credential_lines(stdin.lock())
}

fn read_credential_lines(reader: impl BufRead) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.to_string(), value.to_string());
        }
    }
    Ok(map)
}

fn write_credential(out: &mut impl Write, fields: &BTreeMap<String, String>) -> Result<()> {
    let preferred = [
        "capability[]",
        "authtype",
        "credential",
        "protocol",
        "host",
        "path",
        "username",
        "password",
        "password_expiry_utc",
        "oauth_refresh_token",
    ];
    for key in preferred {
        if let Some(value) = fields.get(key) {
            writeln!(out, "{key}={value}")?;
        }
    }
    for (key, value) in fields {
        if !preferred.contains(&key.as_str()) {
            writeln!(out, "{key}={value}")?;
        }
    }
    writeln!(out)?;
    Ok(())
}

fn password_expired(fields: &BTreeMap<String, String>) -> bool {
    fields
        .get("password_expiry_utc")
        .and_then(|value| value.parse::<i64>().ok())
        .is_some_and(|expiry| time::OffsetDateTime::now_utc().unix_timestamp() >= expiry)
}

fn should_store(fields: &BTreeMap<String, String>) -> bool {
    if fields
        .get("ephemeral")
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
    {
        return false;
    }
    if password_expired(fields) {
        return false;
    }
    fields.contains_key("password") || fields.contains_key("credential")
}

fn sidecar_path(socket: &Path) -> PathBuf {
    socket.with_extension("store")
}

fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode_component(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[idx + 1]), hex_value(bytes[idx + 2])) {
                out.push((hi << 4) | lo);
                idx += 3;
                continue;
            }
        }
        out.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn serialize_cache_entry(entry: &CacheEntry) -> String {
    let ttl = entry
        .expires_at
        .checked_duration_since(Instant::now())
        .unwrap_or_default()
        .as_secs();
    let mut parts = vec![ttl.to_string()];
    for (key, value) in &entry.fields {
        parts.push(format!(
            "{}={}",
            encode_component(key),
            encode_component(value)
        ));
    }
    parts.join("\t")
}

fn deserialize_cache_entry(line: &str) -> Option<CacheEntry> {
    let mut parts = line.split('\t');
    let ttl = parts.next()?.parse::<u64>().ok()?;
    let mut fields = BTreeMap::new();
    for part in parts {
        let (key, value) = part.split_once('=')?;
        fields.insert(decode_component(key), decode_component(value));
    }
    Some(CacheEntry {
        fields,
        expires_at: Instant::now() + Duration::from_secs(ttl),
    })
}

fn read_sidecar_entries(socket: &Path) -> Result<Vec<CacheEntry>> {
    let path = sidecar_path(socket);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    Ok(text.lines().filter_map(deserialize_cache_entry).collect())
}

fn write_sidecar_entries(socket: &Path, entries: &[CacheEntry]) -> Result<()> {
    let path = sidecar_path(socket);
    let mut text = String::new();
    for entry in entries {
        if entry.is_expired() {
            continue;
        }
        text.push_str(&serialize_cache_entry(entry));
        text.push('\n');
    }
    fs::write(&path, text).with_context(|| format!("write {}", path.display()))
}

fn sidecar_get(
    socket: &Path,
    query: &BTreeMap<String, String>,
) -> Result<Option<BTreeMap<String, String>>> {
    let mut entries = read_sidecar_entries(socket)?;
    entries.retain(|entry| !entry.is_expired());
    let out = entries
        .iter()
        .find(|entry| entry.matches_get(query) && entry.usable_for_query(query))
        .map(|entry| entry.fields.clone());
    write_sidecar_entries(socket, &entries)?;
    Ok(out)
}

fn sidecar_store(socket: &Path, creds: BTreeMap<String, String>, timeout: u64) -> Result<()> {
    let mut entries = read_sidecar_entries(socket)?;
    entries.retain(|entry| !entry.is_expired() && !entry.matches_store_identity(&creds));
    if should_store(&creds) {
        entries.push(CacheEntry {
            fields: creds,
            expires_at: Instant::now() + Duration::from_secs(timeout),
        });
    }
    write_sidecar_entries(socket, &entries)
}

fn sidecar_erase(socket: &Path, creds: &BTreeMap<String, String>) -> Result<()> {
    let mut entries = read_sidecar_entries(socket)?;
    entries.retain(|entry| !entry.is_expired() && !entry.matches_erase(creds));
    write_sidecar_entries(socket, &entries)
}

fn default_socket_path() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").context("HOME not set")?);
    let user_dir = home.join(".git-credential-cache");
    if user_dir.exists() {
        return Ok(user_dir.join("socket"));
    }
    let cache_home = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"));
    Ok(cache_home.join("git").join("credential").join("socket"))
}

fn socket_path(raw: Option<&str>) -> Result<PathBuf> {
    let path = raw
        .map(expand_socket_path)
        .map_or_else(default_socket_path, Ok)?;
    if !path.is_absolute() {
        bail!(
            "credential-cache socket path must be absolute: {}",
            path.display()
        );
    }
    Ok(path)
}

fn expand_socket_path(raw: &str) -> PathBuf {
    let Ok(home) = std::env::var("HOME") else {
        return PathBuf::from(raw);
    };
    if raw == "$HOME" {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("$HOME/") {
        return PathBuf::from(home).join(rest);
    }
    if let Some(rest) = raw.strip_prefix("${HOME}/") {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(raw)
}

#[cfg(unix)]
fn send_request(
    socket: &Path,
    action: &str,
    timeout: u64,
    creds: &BTreeMap<String, String>,
) -> Result<Option<BTreeMap<String, String>>> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connect credential cache socket {}", socket.display()))?;
    writeln!(stream, "action={action}")?;
    writeln!(stream, "timeout={timeout}")?;
    writeln!(stream)?;
    write_credential(&mut stream, creds)?;
    stream.flush()?;
    let reader = io::BufReader::new(stream);
    let response = read_credential_lines(reader)?;
    if response.is_empty() {
        Ok(None)
    } else {
        Ok(Some(response))
    }
}

#[cfg(unix)]
fn ensure_daemon(socket: &Path) -> Result<()> {
    if UnixStream::connect(socket).is_ok() {
        return Ok(());
    }
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let exe = std::env::current_exe().context("resolve current executable")?;
    Command::new(exe)
        .arg("credential-cache")
        .arg("daemon")
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start credential-cache daemon")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!(
        "credential-cache daemon did not create socket {}",
        socket.display()
    )
}

#[cfg(unix)]
fn run_client(args: Args) -> Result<()> {
    let path = socket_path(args.socket.as_deref())?;
    if args.action == "exit" {
        if UnixStream::connect(&path).is_ok() {
            let creds = BTreeMap::new();
            let _ = send_request(&path, "exit", args.timeout, &creds);
        }
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(sidecar_path(&path));
        return Ok(());
    }
    let creds = read_credential_from_stdin()?;
    ensure_daemon(&path)?;
    match args.action.as_str() {
        "get" => {
            let response = sidecar_get(&path, &creds)?;
            let stdout = io::stdout();
            let mut out = stdout.lock();
            if let Some(fields) = response {
                write_credential(&mut out, &fields)?;
            } else {
                writeln!(out)?;
            }
        }
        "store" => sidecar_store(&path, creds, args.timeout)?,
        "erase" => sidecar_erase(&path, &creds)?,
        _ => {}
    }
    Ok(())
}

#[cfg(unix)]
fn run_daemon(socket: PathBuf) -> Result<()> {
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("bind credential cache socket {}", socket.display()))?;
    let mut entries: Vec<CacheEntry> = Vec::new();
    for stream in listener.incoming() {
        let stream = stream?;
        if handle_daemon_request(stream, &mut entries)? {
            break;
        }
    }
    let _ = fs::remove_file(&socket);
    Ok(())
}

#[cfg(unix)]
fn handle_daemon_request(mut stream: UnixStream, entries: &mut Vec<CacheEntry>) -> Result<bool> {
    let mut reader = io::BufReader::new(stream.try_clone()?);
    let header = read_credential_lines(&mut reader)?;
    let action = header.get("action").cloned().unwrap_or_default();
    let timeout = header
        .get("timeout")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(900);
    let creds = read_credential_lines(&mut reader)?;
    entries.retain(|entry| !entry.is_expired());
    match action.as_str() {
        "get" => {
            if let Some(entry) = entries
                .iter()
                .find(|entry| entry.matches_get(&creds) && entry.usable_for_query(&creds))
            {
                write_credential(&mut stream, &entry.fields)?;
            } else {
                writeln!(stream)?;
            }
        }
        "store" => {
            if should_store(&creds) {
                entries.retain(|entry| !entry.matches_store_identity(&creds));
                entries.push(CacheEntry {
                    fields: creds,
                    expires_at: Instant::now() + Duration::from_secs(timeout),
                });
            }
            writeln!(stream)?;
        }
        "erase" => {
            entries.retain(|entry| !entry.matches_erase(&creds));
            writeln!(stream)?;
        }
        "exit" => {
            writeln!(stream)?;
            return Ok(true);
        }
        _ => {
            writeln!(stream)?;
        }
    }
    Ok(false)
}

/// Run `grit credential-cache`.
pub fn run(args: Args) -> Result<()> {
    #[cfg(unix)]
    {
        if args.action == "daemon" {
            return run_daemon(socket_path(args.socket.as_deref())?);
        }
        if !matches!(args.action.as_str(), "get" | "store" | "erase" | "exit") {
            bail!("unknown credential-cache action: {}", args.action);
        }
        run_client(args)
    }
    #[cfg(not(unix))]
    {
        if !matches!(args.action.as_str(), "get" | "store" | "erase" | "exit") {
            bail!("unknown credential-cache action: {}", args.action);
        }
        let _ = read_credential_from_stdin()?;
        if args.action == "get" {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            writeln!(out)?;
        }
        Ok(())
    }
}
