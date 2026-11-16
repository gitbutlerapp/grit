//! Smart-HTTP TLS: private CA via `http.sslCAInfo` and `GIT_SSL_NO_VERIFY`.
//!
//! Spins up `openssl s_server` with a self-signed cert, then checks that the
//! default `UreqHttpClient` rejects the handshake unless that cert is configured
//! via `http.sslCAInfo` (or verification is disabled).
//!
//!   cargo test -p grit-lib --features http-ureq --test transport_http_ssl

#![cfg(feature = "http-ureq")]

use grit_lib::config::environment::Environment;
use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::remote::transport::http::ureq_client::UreqHttpClient;
use grit_lib::remote::transport::http::HttpClient;
use grit_test_support::free_port;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct TlsFixture {
    ca_pem: std::path::PathBuf,
    port: u16,
    server: Child,
    _dir: tempfile::TempDir,
}

impl Drop for TlsFixture {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

impl TlsFixture {
    fn new() -> Option<Self> {
        let dir = tempfile::tempdir().ok()?;
        let root = dir.path();
        let ca_key = root.join("ca.key");
        let ca_pem = root.join("ca.pem");
        let server_key = root.join("server.key");
        let server_csr = root.join("server.csr");
        let server_pem = root.join("server.pem");

        let mk_ca = Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-sha256", "-keyout"])
            .arg(&ca_key)
            .args(["-out"])
            .arg(&ca_pem)
            .args([
                "-days",
                "1",
                "-nodes",
                "-subj",
                "/CN=TestCA",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !mk_ca.success() {
            return None;
        }

        let mk_csr = Command::new("openssl")
            .args(["req", "-newkey", "rsa:2048", "-sha256", "-nodes", "-keyout"])
            .arg(&server_key)
            .args(["-out"])
            .arg(&server_csr)
            .args([
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost,IP:127.0.0.1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !mk_csr.success() {
            return None;
        }

        let sign = Command::new("openssl")
            .args(["x509", "-req", "-sha256", "-in"])
            .arg(&server_csr)
            .args(["-CA"])
            .arg(&ca_pem)
            .args(["-CAkey"])
            .arg(&ca_key)
            .args(["-CAcreateserial", "-out"])
            .arg(&server_pem)
            .args(["-days", "1", "-copy_extensions", "copy"])
            .args(["-addext", "basicConstraints=critical,CA:FALSE"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !sign.success() {
            return None;
        }

        let port = free_port()?;
        let server = Command::new("openssl")
            .args(["s_server", "-accept"])
            .arg(port.to_string())
            .args(["-cert"])
            .arg(&server_pem)
            .args(["-key"])
            .arg(&server_key)
            .args(["-www", "-naccept", "16"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(Self {
                    ca_pem,
                    port,
                    server,
                    _dir: dir,
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn url(&self) -> String {
        format!("https://127.0.0.1:{}/", self.port)
    }
}

fn client_from_config(cfg: &ConfigSet, env: &Environment) -> UreqHttpClient {
    UreqHttpClient::from_config(cfg, env).expect("client")
}

#[test]
fn private_ca_rejected_until_ssl_ca_info_configured() {
    let Some(fix) = TlsFixture::new() else {
        eprintln!("openssl TLS fixture unavailable, skipping");
        return;
    };
    let url = fix.url();
    let client = client_from_config(&ConfigSet::new(), &Environment::empty());
    let err = client.get(&url, None).expect_err("unknown CA must fail");
    assert!(
        matches!(err, Error::Remote(_) | Error::Cancelled),
        "unexpected error: {err:?}"
    );

    let mut cfg = ConfigSet::new();
    cfg.add_command_override("http.sslCAInfo", fix.ca_pem.to_str().expect("path"))
        .expect("sslCAInfo");
    let trusted = client_from_config(&cfg, &Environment::empty());
    match trusted.get(&url, None) {
        Ok(_) => {}
        Err(e) => panic!("http.sslCAInfo should trust the private cert: {e:?}"),
    }
}

#[test]
fn git_ssl_no_verify_accepts_private_ca() {
    let Some(fix) = TlsFixture::new() else {
        eprintln!("openssl TLS fixture unavailable, skipping");
        return;
    };
    let url = fix.url();
    let env = Environment::from_vars(
        [("GIT_SSL_NO_VERIFY", "please-ignore")]
            .into_iter()
            .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v))),
        std::path::PathBuf::from("/"),
    );
    let client = client_from_config(&ConfigSet::new(), &env);
    client
        .get(&url, None)
        .expect("GIT_SSL_NO_VERIFY allows TLS");
}
