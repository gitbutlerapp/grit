//! Local smart HTTP servers for network benchmarks.

use std::env;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::binary::resolve_http_server as resolve_server_bin;

/// Running `git http-backend` behind a minimal HTTP/1.1 listener.
pub struct GitHttpBackendServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    _git: PathBuf,
    _project_root: PathBuf,
}

impl GitHttpBackendServer {
    pub fn start(git: &Path, project_root: PathBuf) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("bind git http-backend")?;
        listener
            .set_nonblocking(true)
            .context("set_nonblocking listener")?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let git_path = git.to_path_buf();
        let root = project_root.clone();
        let thread = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let git_path = git_path.clone();
                        let root = root.clone();
                        thread::spawn(move || {
                            let _ = handle_git_http_connection(&git_path, &root, &mut stream);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
            _git: git.to_path_buf(),
            _project_root: project_root,
        })
    }

    pub fn url(&self, repo_name: &str) -> String {
        format!("http://{}/{}", self.addr, repo_name)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for GitHttpBackendServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Handle one HTTP request for `git http-backend` (used by the in-process server and tests).
pub fn handle_git_http_connection(
    git: &Path,
    project_root: &Path,
    stream: &mut TcpStream,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk).context("read HTTP request")?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 256 * 1024 {
            bail!("HTTP request too large");
        }
    }

    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("HTTP header end")?
        + 4;
    let header_bytes = buf[..header_end].to_vec();
    let header_text = String::from_utf8_lossy(&header_bytes);
    let mut lines = header_text.lines();
    let request_line = lines.next().context("request line")?.to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    let mut content_type = String::new();
    let mut expect_continue = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("Content-Length") {
            content_length = value.parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("Content-Type") {
            content_type = value.to_string();
        } else if name.eq_ignore_ascii_case("Expect") && value.eq_ignore_ascii_case("100-continue")
        {
            expect_continue = true;
        }
    }

    if expect_continue {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }

    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).context("read HTTP body")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = &buf[header_end..header_end + content_length];

    let path_info = path.split('?').next().unwrap_or(path.as_str());
    let query = path.split('?').nth(1).unwrap_or("");

    let backend = git_http_backend_path(git)?;
    let mut cmd = Command::new(&backend);
    configure_git_http_backend_cgi(
        &mut cmd,
        project_root,
        path_info,
        query,
        &method,
        content_length,
        &content_type,
    );
    if content_length > 0 {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
    } else {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
    }
    let mut child = cmd.spawn().context("spawn git http-backend")?;
    if content_length > 0 {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(body)?;
        }
    }
    let output = child.wait_with_output().context("wait git http-backend")?;
    if !output.status.success() {
        let response = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
        let _ = stream.write_all(response);
        return Ok(());
    }
    let response = wrap_cgi_response(&output.stdout);
    stream.write_all(&response)?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

/// Turn `git http-backend` CGI output into a complete HTTP/1.1 response.
fn wrap_cgi_response(raw: &[u8]) -> Vec<u8> {
    let Some(sep) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        let mut out = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_vec();
        out.extend_from_slice(raw);
        return out;
    };
    let header_end = sep + 4;
    let headers = &raw[..header_end];
    let body = &raw[header_end..];
    let headers_text = String::from_utf8_lossy(headers);
    let mut lines: Vec<String> = headers_text
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if !lines
        .iter()
        .any(|l| l.to_ascii_lowercase().starts_with("content-length:"))
    {
        lines.push(format!("Content-Length: {}", body.len()));
    }
    if !lines
        .iter()
        .any(|l| l.to_ascii_lowercase().starts_with("connection:"))
    {
        lines.push("Connection: close".to_string());
    }
    let mut out = b"HTTP/1.1 200 OK\r\n".to_vec();
    for line in lines {
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// Hermetic CGI environment for `git http-backend` (inherits only `PATH` and `HOME`).
fn configure_git_http_backend_cgi(
    cmd: &mut Command,
    project_root: &Path,
    path_info: &str,
    query: &str,
    method: &str,
    content_length: usize,
    content_type: &str,
) {
    cmd.env_clear();
    if let Ok(path) = env::var("PATH") {
        cmd.env("PATH", path);
    }
    if let Ok(home) = env::var("HOME") {
        cmd.env("HOME", home);
    }
    cmd.env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_PROJECT_ROOT", project_root)
        .env("PATH_INFO", path_info)
        .env("QUERY_STRING", query)
        .env("REQUEST_METHOD", method)
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("GATEWAY_INTERFACE", "CGI/1.1");
    if content_length > 0 {
        cmd.env("CONTENT_LENGTH", content_length.to_string());
    }
    if !content_type.is_empty() {
        cmd.env("CONTENT_TYPE", content_type);
    }
}

fn git_http_backend_path(git: &Path) -> Result<PathBuf> {
    let out = Command::new(git)
        .arg("--exec-path")
        .output()
        .context("git --exec-path")?;
    if !out.status.success() {
        bail!("git --exec-path failed");
    }
    let exec = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let backend = PathBuf::from(exec).join("git-http-backend");
    if backend.is_file() {
        return Ok(backend);
    }
    find_on_path("git-http-backend").context("locate git-http-backend")
}

fn find_on_path(name: &str) -> Result<PathBuf> {
    let paths = std::env::var_os("PATH").context("PATH unset")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("`{name}` not found on PATH")
}

/// Child process running `grit-http-server`.
pub struct GritHttpServer {
    child: Child,
    addr: SocketAddr,
}

impl GritHttpServer {
    pub fn start(grit: &Path, http_server: Option<&Path>, root: PathBuf) -> Result<Self> {
        let server_bin = resolve_server_bin(http_server)?;
        let listener = TcpListener::bind("127.0.0.1:0").context("bind grit-http-server")?;
        let addr = listener.local_addr()?;
        drop(listener);

        let mut child = Command::new(&server_bin)
            .arg("--root")
            .arg(&root)
            .arg("--bind")
            .arg(addr.to_string())
            .env("GUST_BIN", grit)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn {}", server_bin.display()))?;

        if !wait_tcp(addr, Duration::from_secs(15)) {
            let _ = child.kill();
            bail!("grit-http-server did not accept connections on {addr}");
        }
        Ok(Self { child, addr })
    }

    pub fn url(&self, repo_name: &str) -> String {
        format!("http://{}/{}", self.addr, repo_name)
    }
}

impl Drop for GritHttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn wrap_cgi_adds_status_and_content_length() {
        let raw = b"Content-Type: text/plain\r\n\r\nhello";
        let wrapped = wrap_cgi_response(raw);
        let text = String::from_utf8_lossy(&wrapped);
        assert!(text.starts_with("HTTP/1.1 200 OK"));
        assert!(text.contains("Content-Length: 5"));
        assert!(text.ends_with("hello"));
    }

    #[test]
    fn git_http_backend_serves_cloneable_repo() {
        let git = PathBuf::from("/usr/bin/git");
        let tmp = tempfile::tempdir().expect("tempdir");
        let work = tmp.path();
        let bare = work.join("upstream.git");
        Command::new(&git)
            .current_dir(work)
            .args(["init", "--bare", bare.to_str().unwrap()])
            .status()
            .expect("init bare");
        let seed = work.join("seed");
        Command::new(&git)
            .current_dir(work)
            .args(["init", "-q", seed.to_str().unwrap()])
            .status()
            .expect("init seed");
        std::fs::write(seed.join("README"), b"network bench smoke\n").unwrap();
        let seed_steps: &[&[&str]] = &[
            &["add", "README"],
            &["commit", "-m", "seed"],
            &["branch", "-M", "main"],
            &["remote", "add", "origin", bare.to_str().unwrap()],
            &["push", "-q", "origin", "main"],
        ];
        for args in seed_steps {
            let status = Command::new(&git)
                .current_dir(&seed)
                .args(["-c", "user.email=bench@test", "-c", "user.name=bench"])
                .args(*args)
                .status()
                .expect("seed repo setup");
            assert!(status.success(), "seed repo setup step failed");
        }
        let root = work.join("http");
        std::fs::create_dir_all(&root).unwrap();
        Command::new(&git)
            .current_dir(work)
            .args([
                "clone",
                "--bare",
                bare.to_str().unwrap(),
                root.join("fixture.git").to_str().unwrap(),
            ])
            .status()
            .expect("clone bare");
        let server = GitHttpBackendServer::start(&git, root.clone()).expect("start server");
        let url = server.url("fixture.git");
        let dest = work.join("clone");
        let status = Command::new(&git)
            .current_dir(work)
            .args(["clone", "-q", &url, dest.to_str().unwrap()])
            .status()
            .expect("git clone");
        assert!(status.success(), "git clone over git-http-backend failed");
    }
}

pub fn wait_tcp(addr: SocketAddr, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}
