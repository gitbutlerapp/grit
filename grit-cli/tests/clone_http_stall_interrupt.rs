//! Issue #2469: `grit clone` against a stalled HTTP server exits promptly on signals.

#![cfg(unix)]

mod interrupt_common;

use std::io::Read;
use std::net::TcpListener;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use grit_test_support::free_port;
use interrupt_common::{sighup, sigint, sigterm, spawn_grit};

fn spawn_stall_server(port: u16, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let listener = TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind");
        listener.set_nonblocking(true).ok();
        while !stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut buf = [0u8; 65536];
            let _ = stream.read(&mut buf);
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(50));
            }
        }
    })
}

fn assert_exits_within(child: &mut Child, within: Duration) -> i32 {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status.code().unwrap_or(-1);
        }
        if Instant::now() >= deadline {
            panic!("grit clone still running after {:?}", within);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn clone_against_stall(port: Option<u16>, deliver: fn(&Child)) {
    let port = port.expect("free port");
    let base = tempfile::tempdir().expect("tempdir");
    let dest = base.path().join("cl");
    let url = format!("http://127.0.0.1:{port}/x.git");

    let stop = Arc::new(AtomicBool::new(false));
    let server = spawn_stall_server(port, Arc::clone(&stop));
    let mut child = spawn_grit(base.path(), &["clone", &url, dest.to_str().expect("dest")]);
    thread::sleep(Duration::from_millis(100));
    deliver(&child);
    let code = assert_exits_within(&mut child, Duration::from_secs(3));
    stop.store(true, Ordering::Relaxed);
    let _ = server.join();
    assert_eq!(code, 130, "expected interrupt exit 130, got {code}");
}

#[test]
fn sigint_sigterm_sighup_during_http_clone_against_stalled_server_exit_promptly() {
    let port = free_port();
    clone_against_stall(port, sigint);
    clone_against_stall(free_port(), sigterm);
    clone_against_stall(free_port(), sighup);
}
