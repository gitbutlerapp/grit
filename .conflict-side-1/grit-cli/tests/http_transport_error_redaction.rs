//! HTTP transport errors must redact URL credentials but preserve `@` in paths.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn spawn_http_404_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp);
        }
    });
    port
}

fn assert_redacted_404_output(extra_args: &[&str], expect_stdout: bool) {
    let port = spawn_http_404_server();
    let url = format!(
        "http://x-access-token:SECRETTOKEN@127.0.0.1:{port}/org/repo@v1.git/info/refs?service=git-upload-pack"
    );
    let out = Command::new(GRIT)
        .args(extra_args)
        .args(["remote", "refs", &url])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert!(
        !out.status.success(),
        "expected failure, got stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = if expect_stdout {
        format!("{stdout}{stderr}")
    } else {
        format!("{stderr}{stdout}")
    };
    assert!(
        !combined.contains("SECRETTOKEN"),
        "credential leaked: {combined}"
    );
    assert!(
        !combined.contains("x-access-token"),
        "credential username leaked: {combined}"
    );
    assert!(
        combined.contains("repo@v1.git"),
        "path @ must be preserved: {combined}"
    );
    assert!(
        combined.contains("404"),
        "expected HTTP status in message: {combined}"
    );
}

#[test]
fn http_404_human_json_and_markdown_redact_url_credentials() {
    assert_redacted_404_output(&[], false);
    assert_redacted_404_output(&["--json"], true);
    assert_redacted_404_output(&["--markdown"], true);
}
