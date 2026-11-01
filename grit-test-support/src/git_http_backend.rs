//! The system `git http-backend` served over localhost HTTP/1.1, so tests can run
//! grit's smart-HTTP client against Git's own server.
//!
//! Each request runs one `git http-backend` CGI process with a hermetic
//! environment (no system or global config). Request headers become `HTTP_*`
//! variables, so `Git-Protocol: version=2` and gzip request bodies reach the
//! backend as they would behind a real web server.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

/// A running `git http-backend` server; stops when dropped.
pub struct GitHttpBackend {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<thread::JoinHandle<()>>,
}

impl GitHttpBackend {
    /// Serve every repository under `project_root` at `http://<addr>/<name>`.
    ///
    /// # Errors
    ///
    /// When no localhost port can be bound or `git http-backend` cannot be found.
    pub fn start(project_root: &Path) -> io::Result<Self> {
        let backend = backend_path()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let root = project_root.to_path_buf();
        let accept = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    continue;
                };
                let (backend, root) = (backend.clone(), root.clone());
                thread::spawn(move || {
                    let _ = serve(&backend, &root, &mut stream);
                });
            }
        });
        Ok(Self {
            addr,
            stop,
            accept: Some(accept),
        })
    }

    /// URL of the repository `name` (a directory under the project root).
    #[must_use]
    pub fn url(&self, name: &str) -> String {
        format!("http://{}/{name}", self.addr)
    }
}

impl Drop for GitHttpBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the blocking accept so the thread sees the stop flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn backend_path() -> io::Result<PathBuf> {
    let out = Command::new("git").arg("--exec-path").output()?;
    if !out.status.success() {
        return Err(io::Error::other("git --exec-path failed"));
    }
    let backend =
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).join("git-http-backend");
    if backend.is_file() {
        Ok(backend)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "git-http-backend not found",
        ))
    }
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn read_request(reader: &mut impl BufRead, stream: &mut TcpStream) -> io::Result<Request> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_owned();
    let path = parts.next().unwrap_or("/").to_owned();
    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let mut request = Request {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    if request
        .header("Expect")
        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
    {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    if request
        .header("Transfer-Encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        request.body = read_chunked(reader)?;
    } else {
        let len = request
            .header("Content-Length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        request.body = vec![0; len];
        reader.read_exact(&mut request.body)?;
    }
    Ok(request)
}

fn read_chunked(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let size_hex = line.trim().split(';').next().unwrap_or("0");
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            loop {
                line.clear();
                if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        line.clear();
        reader.read_line(&mut line)?;
    }
}

fn serve(backend: &Path, root: &Path, stream: &mut TcpStream) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let request = read_request(&mut reader, stream)?;
    let (path_info, query) = request
        .path
        .split_once('?')
        .unwrap_or((request.path.as_str(), ""));

    let mut cmd = Command::new(backend);
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_PROJECT_ROOT", root)
        .env("PATH_INFO", path_info)
        .env("QUERY_STRING", query)
        .env("REQUEST_METHOD", &request.method)
        .env("CONTENT_LENGTH", request.body.len().to_string())
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("REMOTE_USER", "test");
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("Content-Type") {
            cmd.env("CONTENT_TYPE", value);
        } else if !name.eq_ignore_ascii_case("Content-Length")
            && !name.eq_ignore_ascii_case("Transfer-Encoding")
        {
            cmd.env(
                format!("HTTP_{}", name.to_ascii_uppercase().replace('-', "_")),
                value,
            );
        }
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("http-backend stdin"))?;
    let body = request.body;
    // A separate writer keeps a large request and a large reply from deadlocking.
    let writer = thread::spawn(move || stdin.write_all(&body));
    let mut stdout = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("http-backend stdout"))?,
    );

    let mut status = "200 OK".to_owned();
    let mut head = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if stdout.read_line(&mut line)? == 0 {
            break;
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        match header.split_once(':') {
            Some((name, value)) if name.eq_ignore_ascii_case("Status") => {
                status = value.trim().to_owned();
            }
            Some(_) => {
                head.push_str(header);
                head.push_str("\r\n");
            }
            None => {}
        }
    }
    // The CGI gives no length: the body runs to the end of the connection.
    write!(
        stream,
        "HTTP/1.1 {status}\r\n{head}Connection: close\r\n\r\n"
    )?;
    io::copy(&mut stdout, stream)?;
    let _ = writer.join();
    let _ = child.wait();
    stream.shutdown(std::net::Shutdown::Write)
}

#[cfg(all(test, feature = "full-tests"))]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::GitHttpBackend;
    use std::process::Command;

    #[test]
    fn serves_a_cloneable_repository() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("repo.git");
        let status = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&bare)
            .status()
            .expect("git init");
        assert!(status.success());
        let server = GitHttpBackend::start(tmp.path()).expect("start");
        let out = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["ls-remote", &server.url("repo.git")])
            .output()
            .expect("ls-remote");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
