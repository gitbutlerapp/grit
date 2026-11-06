//! HTTP transport cancellation tests (issue #2469).

#![cfg(all(test, feature = "http-ureq"))]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::clone::{clone, CloneError, CloneOptions};
use crate::config::ConfigSet;
use crate::environment::Environment;
use crate::error::Error;
use crate::pkt_line::write_line_to_vec;
use crate::progress::NullProgress;
use crate::progress::{connect_cancel_arc, mirror_stack_cancel, CancelToken, HttpWaitGuard};
use crate::remote::fetch::NoProgress;
use crate::remote::transfer::FetchOptions;
use crate::remote::{DefaultHttpClientFactory, Remote};
use crate::repo::{InitOptions, Repository};
use crate::transport::http::http_client_arc;
use crate::transport::http::ureq_client::UreqHttpClient;
use crate::transport::http::SmartHttpTransport;
use crate::transport::{ConnectOptions, Service, Transport};

const TEST_HARD_TIMEOUT: Duration = Duration::from_secs(5);
const SLOW_DISCOVERY_HARD_TIMEOUT: Duration = Duration::from_secs(10);

fn run_with_hard_timeout<T: Send + 'static>(
    label: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    run_with_hard_timeout_for(label, TEST_HARD_TIMEOUT, f)
}

fn run_with_hard_timeout_for<T: Send + 'static>(
    label: &str,
    limit: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit)
        .unwrap_or_else(|_| panic!("{label} did not finish within {limit:?}"))
}

/// Serves one slow `info/refs` body (2 B / 100 ms), then 404 on any further connection.
fn spawn_slow_info_refs_server(body: Vec<u8>) -> (u16, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_server = Arc::clone(&requests);
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow server");
        port_tx
            .send(listener.local_addr().expect("addr").port())
            .expect("port");
        listener.set_nonblocking(true).ok();
        let body = body;
        loop {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            let n = requests_server.fetch_add(1, Ordering::SeqCst) + 1;
            if n > 1 {
                stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .ok();
                continue;
            }
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let Ok(read) = stream.read(&mut tmp) else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..read]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-advertisement\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).expect("header");
            let mut sent = 0usize;
            while sent < body.len() {
                thread::sleep(Duration::from_millis(100));
                let chunk = (body.len() - sent).min(2);
                stream.write_all(&body[sent..sent + chunk]).expect("body");
                sent += chunk;
            }
        }
    });
    let port = port_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("server port");
    (port, requests)
}

#[test]
fn discover_cancels_while_info_refs_stalls() {
    run_with_hard_timeout("discover_cancels_while_info_refs_stalls", || {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_server = Arc::clone(&stop);
        let listener = Arc::new(listener);
        let accept_listener = Arc::clone(&listener);
        thread::spawn(move || {
            while !stop_server.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = accept_listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let stop_conn = Arc::clone(&stop_server);
                thread::spawn(move || {
                    let mut buf = [0u8; 65536];
                    let _ = stream.read(&mut buf);
                    while !stop_conn.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(50));
                    }
                });
            }
        });

        let cancel = CancelToken::new();
        let cancel_arm = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            cancel_arm.cancel();
        });

        let url = format!("http://127.0.0.1:{port}/x.git");
        let transport = SmartHttpTransport::new(http_client_arc(UreqHttpClient::new()));
        let (stack_watch, _guard) = HttpWaitGuard::bind(&cancel);
        let mut connect = ConnectOptions {
            cancel: Some(connect_cancel_arc(
                &cancel,
                std::sync::Arc::clone(&stack_watch),
            )),
            stack_watch,
            ..Default::default()
        };
        connect.bind_stack_cancel(&cancel);
        let started = Instant::now();
        let err = match transport.connect(&url, Service::UploadPack, &connect) {
            Ok(_) => panic!("stalling discovery must not succeed"),
            Err(e) => e,
        };
        stop.store(true, Ordering::Relaxed);
        drop(listener);
        assert!(
            matches!(err, Error::Cancelled),
            "expected Cancelled, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancel took {:?}",
            started.elapsed()
        );
    });
}

#[test]
fn mirror_stack_cancel_reflects_deadline_cancel() {
    let cancel = deadline_cancel_token(Instant::now() + Duration::from_millis(50));
    let watch = std::sync::atomic::AtomicBool::new(false);
    thread::sleep(Duration::from_millis(80));
    mirror_stack_cancel(&cancel, &watch);
    assert!(
        watch.load(Ordering::Relaxed),
        "deadline cancel must mirror into stack watch"
    );
}

fn deadline_cancel_token(deadline: Instant) -> CancelToken {
    let token = CancelToken::new();
    if Instant::now() >= deadline {
        token.cancel();
        return token;
    }
    let cancel = token.clone();
    thread::spawn(move || {
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        cancel.cancel();
    });
    token
}

#[test]
fn discover_cancels_with_deadline_cancel_without_shared_watch() {
    run_with_hard_timeout(
        "discover_cancels_with_deadline_cancel_without_shared_watch",
        || {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall server");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let port = listener.local_addr().expect("addr").port();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_server = Arc::clone(&stop);
            let listener = Arc::new(listener);
            let accept_listener = Arc::clone(&listener);
            thread::spawn(move || {
                while !stop_server.load(Ordering::Relaxed) {
                    let Ok((mut stream, _)) = accept_listener.accept() else {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let stop_conn = Arc::clone(&stop_server);
                    thread::spawn(move || {
                        let mut buf = [0u8; 65536];
                        let _ = stream.read(&mut buf);
                        while !stop_conn.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(50));
                        }
                    });
                }
            });

            let cancel = deadline_cancel_token(Instant::now() + Duration::from_millis(300));
            let url = format!("http://127.0.0.1:{port}/x.git");
            let transport = SmartHttpTransport::new(http_client_arc(UreqHttpClient::new()));
            let (stack_watch, _guard) = HttpWaitGuard::bind(&cancel);
            let mut connect = ConnectOptions {
                cancel: Some(connect_cancel_arc(
                    &cancel,
                    std::sync::Arc::clone(&stack_watch),
                )),
                stack_watch,
                ..Default::default()
            };
            connect.bind_stack_cancel(&cancel);
            let started = Instant::now();
            let err =
                match transport.connect_with_owner(&url, Service::UploadPack, &connect, &cancel) {
                    Ok(_) => panic!("stalling discovery must not succeed"),
                    Err(e) => e,
                };
            stop.store(true, Ordering::Relaxed);
            drop(listener);
            assert!(
                matches!(err, Error::Cancelled),
                "expected Cancelled, got {err:?}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "cancel took {:?}",
                started.elapsed()
            );
        },
    );
}

#[test]
fn remote_fetch_cancels_during_stalled_discovery_without_shared_watch() {
    run_with_hard_timeout(
        "remote_fetch_cancels_during_stalled_discovery_without_shared_watch",
        || {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall server");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let port = listener.local_addr().expect("addr").port();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_server = Arc::clone(&stop);
            let listener = Arc::new(listener);
            let accept_listener = Arc::clone(&listener);
            thread::spawn(move || {
                while !stop_server.load(Ordering::Relaxed) {
                    let Ok((mut stream, _)) = accept_listener.accept() else {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    let stop_conn = Arc::clone(&stop_server);
                    thread::spawn(move || {
                        let mut buf = [0u8; 65536];
                        let _ = stream.read(&mut buf);
                        while !stop_conn.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(50));
                        }
                    });
                }
            });

            let cancel = deadline_cancel_token(Instant::now() + Duration::from_millis(300));
            let tmp = tempfile::tempdir().expect("tempdir");
            let (repo, _) = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
            let url = format!("http://127.0.0.1:{port}/x.git");
            let remote = Remote::from_url(&url).expect("remote url");
            let started = Instant::now();
            let err = remote
                .fetch(
                    &repo,
                    FetchOptions::default(),
                    &mut NoProgress,
                    &cancel,
                    Some(&DefaultHttpClientFactory::new()),
                )
                .expect_err("stalled discovery must not succeed");
            stop.store(true, Ordering::Relaxed);
            drop(listener);
            assert!(err.is_cancelled(), "expected Cancelled, got {err:?}");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "cancel took {:?}",
                started.elapsed()
            );
        },
    );
}

fn minimal_upload_pack_advertisement() -> Vec<u8> {
    let oid = "a".repeat(40);
    let ref_line = format!("{oid} refs/heads/main\0multi_ack side-band-64k thin-pack ofs-delta");
    let mut body = Vec::new();
    write_line_to_vec(&mut body, "# service=git-upload-pack").expect("line");
    body.extend_from_slice(b"0000");
    write_line_to_vec(&mut body, &ref_line).expect("ref");
    body.extend_from_slice(b"0000");
    body
}

fn write_http_response(stream: &mut TcpStream, content_type: &str, body: &[u8]) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).expect("header");
    stream.write_all(body).expect("body");
}

fn handle_upload_pack_stall_server(mut stream: TcpStream, ad: Vec<u8>, stop: Arc<AtomicBool>) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let Ok(n) = stream.read(&mut tmp) else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let req = String::from_utf8_lossy(&buf);
    if req.starts_with("GET ") && req.contains("/info/refs") {
        write_http_response(
            &mut stream,
            "application/x-git-upload-pack-advertisement",
            &ad,
        );
        return;
    }
    if req.starts_with("POST ") && req.contains("/git-upload-pack") {
        if let Some(len_start) = req.find("Content-Length: ") {
            let rest = &req[len_start + "Content-Length: ".len()..];
            if let Some(clen) = rest
                .split("\r\n")
                .next()
                .and_then(|s| s.parse::<usize>().ok())
            {
                let header_end = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|i| i + 4)
                    .unwrap_or(buf.len());
                while buf.len().saturating_sub(header_end) < clen {
                    let Ok(n) = stream.read(&mut tmp) else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
            }
        }
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn remote_fetch_cancels_during_stalled_upload_pack_post_without_shared_watch() {
    run_with_hard_timeout(
        "remote_fetch_cancels_during_stalled_upload_pack_post_without_shared_watch",
        || {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall server");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let port = listener.local_addr().expect("addr").port();
            let ad = minimal_upload_pack_advertisement();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_server = Arc::clone(&stop);
            let listener = Arc::new(listener);
            let accept_listener = Arc::clone(&listener);
            thread::spawn(move || {
                while !stop_server.load(Ordering::Relaxed) {
                    let Ok((stream, _)) = accept_listener.accept() else {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    let ad = ad.clone();
                    let stop_conn = Arc::clone(&stop_server);
                    thread::spawn(move || handle_upload_pack_stall_server(stream, ad, stop_conn));
                }
            });

            let cancel = deadline_cancel_token(Instant::now() + Duration::from_millis(300));
            let tmp = tempfile::tempdir().expect("tempdir");
            let (repo, _) = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
            let url = format!("http://127.0.0.1:{port}/x.git");
            let remote = Remote::from_url(&url).expect("remote url");
            let started = Instant::now();
            let err = remote
                .fetch(
                    &repo,
                    FetchOptions {
                        http_protocol_version: Some(0),
                        ..FetchOptions::default()
                    },
                    &mut NoProgress,
                    &cancel,
                    Some(&DefaultHttpClientFactory::new()),
                )
                .expect_err("stalled upload-pack POST must not succeed");
            stop.store(true, Ordering::Relaxed);
            drop(listener);
            assert!(err.is_cancelled(), "expected Cancelled, got {err:?}");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "cancel took {:?}",
                started.elapsed()
            );
        },
    );
}

#[test]
fn stateless_upload_pack_post_wait_cancels_without_full_fetch() {
    run_with_hard_timeout(
        "stateless_upload_pack_post_wait_cancels_without_full_fetch",
        || {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall server");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let port = listener.local_addr().expect("addr").port();
            let ad = minimal_upload_pack_advertisement();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_server = Arc::clone(&stop);
            let listener = Arc::new(listener);
            let accept_listener = Arc::clone(&listener);
            thread::spawn(move || {
                while !stop_server.load(Ordering::Relaxed) {
                    let Ok((stream, _)) = accept_listener.accept() else {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    let ad = ad.clone();
                    let stop_conn = Arc::clone(&stop_server);
                    thread::spawn(move || handle_upload_pack_stall_server(stream, ad, stop_conn));
                }
            });

            let cancel = deadline_cancel_token(Instant::now() + Duration::from_millis(300));
            let url = format!("http://127.0.0.1:{port}/x.git");
            let transport = SmartHttpTransport::new(http_client_arc(UreqHttpClient::new()));
            let (stack_watch, _guard) = HttpWaitGuard::bind(&cancel);
            let mut connect = ConnectOptions {
                cancel: Some(connect_cancel_arc(
                    &cancel,
                    std::sync::Arc::clone(&stack_watch),
                )),
                stack_watch,
                ..Default::default()
            };
            connect.bind_stack_cancel(&cancel);
            let started = Instant::now();
            let mut conn = transport
                .connect_with_owner(&url, Service::UploadPack, &connect, &cancel)
                .expect("connect");
            let wants = b"0032want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n0000";
            conn.writer().write_all(wants).expect("write");
            conn.writer().flush().expect("flush");
            let err = conn
                .drain_pending_http(&cancel)
                .expect_err("stalled POST must not succeed");
            stop.store(true, Ordering::Relaxed);
            drop(listener);
            assert!(
                matches!(err, Error::Cancelled),
                "expected Cancelled, got {err:?}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "cancel took {:?}",
                started.elapsed()
            );
        },
    );
}

#[test]
fn clone_cancels_during_stalled_bundle_uri_probe() {
    run_with_hard_timeout("clone_cancels_during_stalled_bundle_uri_probe", || {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_server = Arc::clone(&stop);
        let listener = Arc::new(listener);
        let accept_listener = Arc::clone(&listener);
        thread::spawn(move || {
            while !stop_server.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = accept_listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let stop_conn = Arc::clone(&stop_server);
                thread::spawn(move || {
                    let mut buf = [0u8; 65536];
                    let _ = stream.read(&mut buf);
                    while !stop_conn.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(50));
                    }
                });
            }
        });

        let cancel = CancelToken::new();
        let cancel_arm = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            cancel_arm.cancel();
        });

        let tmp = tempfile::tempdir().expect("tempdir");
        let dest = tmp.path().join("cl");
        let cache = tmp.path().join("bundle-cache");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let url = format!("http://127.0.0.1:{port}/x.git");
        let started = Instant::now();
        let err = clone(
            CloneOptions {
                bundle_cache: Some(cache),
                checkout: false,
                ..CloneOptions::new(url.clone(), dest)
            },
            &mut NoProgress,
            &cancel,
            Some(&DefaultHttpClientFactory::new()),
        )
        .expect_err("stalled bundle probe must not succeed");
        stop.store(true, Ordering::Relaxed);
        drop(listener);
        assert!(err.is_cancelled(), "expected Cancelled, got {err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancel took {:?}",
            started.elapsed()
        );
    });
}

#[test]
fn clone_times_out_on_low_speed_stall_without_cancel() {
    run_with_hard_timeout("clone_times_out_on_low_speed_stall_without_cancel", || {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_server = Arc::clone(&stop);
        let listener = Arc::new(listener);
        let accept_listener = Arc::clone(&listener);
        thread::spawn(move || {
            while !stop_server.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = accept_listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let stop_conn = Arc::clone(&stop_server);
                thread::spawn(move || {
                    let mut buf = [0u8; 65536];
                    let _ = stream.read(&mut buf);
                    while !stop_conn.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(50));
                    }
                });
            }
        });

        let tmp = tempfile::tempdir().expect("tempdir");
        let dest = tmp.path().join("cl");
        let cache = tmp.path().join("bundle-cache");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let url = format!("http://127.0.0.1:{port}/x.git");
        let env = Environment::empty()
            .with_var("GIT_HTTP_LOW_SPEED_LIMIT", "1")
            .with_var("GIT_HTTP_LOW_SPEED_TIME", "1");
        let factory = DefaultHttpClientFactory::with_environment(env.clone());
        let started = Instant::now();
        let err = clone(
            CloneOptions {
                bundle_cache: Some(cache),
                checkout: false,
                environment: env,
                ..CloneOptions::new(url, dest)
            },
            &mut NoProgress,
            &CancelToken::never(),
            Some(&factory),
        )
        .expect_err("low-speed stall must not succeed");
        stop.store(true, Ordering::Relaxed);
        drop(listener);
        assert!(
            matches!(err, CloneError::Remote(_) | CloneError::Nested(_)),
            "expected transport failure, got {err:?}"
        );
        assert!(
            !err.is_cancelled(),
            "low-speed timeout must not look like cancel: {err:?}"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "low-speed timeout took {:?}",
            elapsed
        );
        assert!(
            elapsed >= Duration::from_millis(900),
            "expected ~1s low-speed window, got {:?}",
            elapsed
        );
    });
}

#[test]
fn smart_http_discovery_completes_one_connection_with_slow_body_and_low_speed_env() {
    run_with_hard_timeout_for(
        "smart_http_discovery_completes_one_connection_with_slow_body_and_low_speed_env",
        SLOW_DISCOVERY_HARD_TIMEOUT,
        || {
            let ad = minimal_upload_pack_advertisement();
            let (port, requests) = spawn_slow_info_refs_server(ad);
            let env = Environment::empty()
                .with_var("GIT_HTTP_LOW_SPEED_LIMIT", "1")
                .with_var("GIT_HTTP_LOW_SPEED_TIME", "1");
            let client = UreqHttpClient::from_config_with_environment(&ConfigSet::default(), &env)
                .expect("client");
            let url = format!("http://127.0.0.1:{port}/x.git");
            let transport = SmartHttpTransport::new(http_client_arc(client));
            let cancel = CancelToken::new();
            let (stack_watch, _guard) = HttpWaitGuard::bind(&cancel);
            let mut connect = ConnectOptions {
                cancel: Some(connect_cancel_arc(
                    &cancel,
                    std::sync::Arc::clone(&stack_watch),
                )),
                stack_watch,
                ..Default::default()
            };
            connect.bind_stack_cancel(&cancel);
            let started = Instant::now();
            let conn = transport
                .connect(&url, Service::UploadPack, &connect)
                .expect("slow discovery via PendingHttp must succeed");
            assert!(
                !conn.advertised_refs().is_empty(),
                "expected parsed upload-pack advertisement"
            );
            assert!(
                started.elapsed() >= Duration::from_millis(1500),
                "expected multi-second body read, got {:?}",
                started.elapsed()
            );
            assert_eq!(
                requests.load(Ordering::SeqCst),
                1,
                "must not open a second discovery GET after lowSpeedTime"
            );
        },
    );
}

#[derive(Clone, Copy)]
enum SilentStallMode {
    Silent,
    HeadersContentLength,
}

fn spawn_silent_stall_server(mode: SilentStallMode) -> (u16, Arc<AtomicBool>, Arc<AtomicUsize>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_server = Arc::clone(&stop);
    let connections = Arc::new(AtomicUsize::new(0));
    let connections_server = Arc::clone(&connections);
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind silent stall server");
        port_tx
            .send(listener.local_addr().expect("addr").port())
            .expect("port");
        listener.set_nonblocking(true).ok();
        while !stop_server.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            connections_server.fetch_add(1, Ordering::SeqCst);
            thread::spawn(move || {
                stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
                let mut buf = [0u8; 65536];
                let _ = stream.read(&mut buf);
                if matches!(mode, SilentStallMode::HeadersContentLength) {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-advertisement\r\nContent-Length: 100000\r\n\r\n",
                        )
                        .ok();
                }
                loop {
                    thread::sleep(Duration::from_secs(100));
                }
            });
        }
    });
    let port = port_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("stall server port");
    (port, stop, connections)
}

fn low_speed_test_env() -> Environment {
    Environment::empty()
        .with_var("GIT_HTTP_LOW_SPEED_LIMIT", "1000")
        .with_var("GIT_HTTP_LOW_SPEED_TIME", "1")
}

fn assert_low_speed_transport(err: &Error) {
    let msg = format!("{err}");
    assert!(
        msg.contains("HTTP transfer speed below http.lowSpeedLimit"),
        "expected low-speed transport error, got {msg}"
    );
    assert!(!err.is_cancelled(), "low-speed must not look like cancel");
}

fn assert_low_speed_elapsed(started: Instant) {
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "low-speed timeout took too long: {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "expected ~1s low-speed window, got {elapsed:?}"
    );
}

#[test]
fn smart_http_connect_aborts_on_silent_stall_with_low_speed_env() {
    run_with_hard_timeout(
        "smart_http_connect_aborts_on_silent_stall_with_low_speed_env",
        || {
            let (port, stop, _connections) = spawn_silent_stall_server(SilentStallMode::Silent);
            let env = low_speed_test_env();
            let client = UreqHttpClient::from_config_with_environment(&ConfigSet::default(), &env)
                .expect("client");
            let url = format!("http://127.0.0.1:{port}/x.git");
            let transport = SmartHttpTransport::new(http_client_arc(client));
            let started = Instant::now();
            let err = match transport.connect(&url, Service::UploadPack, &ConnectOptions::default())
            {
                Ok(_) => panic!("silent stall must not succeed"),
                Err(e) => e,
            };
            stop.store(true, Ordering::Relaxed);
            assert_low_speed_transport(&err);
            assert_low_speed_elapsed(started);
        },
    );
}

#[test]
fn smart_http_connect_aborts_on_headers_stall_with_low_speed_env() {
    run_with_hard_timeout(
        "smart_http_connect_aborts_on_headers_stall_with_low_speed_env",
        || {
            let (port, stop, _connections) =
                spawn_silent_stall_server(SilentStallMode::HeadersContentLength);
            let env = low_speed_test_env();
            let client = UreqHttpClient::from_config_with_environment(&ConfigSet::default(), &env)
                .expect("client");
            let url = format!("http://127.0.0.1:{port}/x.git");
            let transport = SmartHttpTransport::new(http_client_arc(client));
            let started = Instant::now();
            let err = match transport.connect(&url, Service::UploadPack, &ConnectOptions::default())
            {
                Ok(_) => panic!("header stall must not succeed"),
                Err(e) => e,
            };
            stop.store(true, Ordering::Relaxed);
            assert_low_speed_transport(&err);
            assert_low_speed_elapsed(started);
        },
    );
}

#[test]
fn clone_aborts_on_silent_stall_with_low_speed_env_and_bundle_cache() {
    run_with_hard_timeout(
        "clone_aborts_on_silent_stall_with_low_speed_env_and_bundle_cache",
        || {
            let (port, stop, connections) = spawn_silent_stall_server(SilentStallMode::Silent);
            let env = low_speed_test_env();
            let factory = DefaultHttpClientFactory::with_environment(env.clone());
            let tmp = tempfile::tempdir().expect("tempdir");
            let dest = tmp.path().join("cl");
            let cache = tmp.path().join("bundle-cache");
            std::fs::create_dir_all(&cache).expect("cache dir");
            let url = format!("http://127.0.0.1:{port}/x.git");
            let started = Instant::now();
            let err = clone(
                CloneOptions {
                    bundle_cache: Some(cache),
                    checkout: false,
                    environment: env,
                    ..CloneOptions::new(url, dest)
                },
                &mut NoProgress,
                &CancelToken::never(),
                Some(&factory),
            )
            .expect_err("silent clone stall must fail");
            stop.store(true, Ordering::Relaxed);
            assert!(
                matches!(err, CloneError::Remote(_) | CloneError::Nested(_)),
                "expected transport failure, got {err:?}"
            );
            assert!(!err.is_cancelled());
            let msg = format!("{err}");
            assert!(msg.contains("HTTP transfer speed below http.lowSpeedLimit"));
            assert_low_speed_elapsed(started);
            assert_eq!(
                connections.load(Ordering::SeqCst),
                1,
                "clone must not open a second HTTP connection after a low-speed bundle probe failure"
            );
        },
    );
}

#[test]
fn remote_fetch_aborts_on_silent_stall_with_low_speed_env() {
    run_with_hard_timeout(
        "remote_fetch_aborts_on_silent_stall_with_low_speed_env",
        || {
            let (port, stop, _connections) = spawn_silent_stall_server(SilentStallMode::Silent);
            let env = low_speed_test_env();
            let tmp = tempfile::tempdir().expect("tempdir");
            let (repo, _) = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
            let url = format!("http://127.0.0.1:{port}/x.git");
            let remote = Remote::from_url(&url).expect("remote url");
            let started = Instant::now();
            let err = remote
                .fetch(
                    &repo,
                    FetchOptions {
                        environment: env,
                        ..FetchOptions::default()
                    },
                    &mut NoProgress,
                    &CancelToken::never(),
                    Some(&DefaultHttpClientFactory::with_environment(
                        low_speed_test_env(),
                    )),
                )
                .expect_err("silent fetch stall must fail");
            stop.store(true, Ordering::Relaxed);
            assert!(!err.is_cancelled(), "low-speed must not look like cancel");
            let msg = format!("{err}");
            assert!(
                msg.contains("HTTP transfer speed below http.lowSpeedLimit"),
                "expected low-speed transport error, got {msg}"
            );
            assert_low_speed_elapsed(started);
        },
    );
}

#[test]
fn remote_fetch_aborts_on_headers_stall_with_low_speed_env() {
    run_with_hard_timeout(
        "remote_fetch_aborts_on_headers_stall_with_low_speed_env",
        || {
            let (port, stop, _connections) =
                spawn_silent_stall_server(SilentStallMode::HeadersContentLength);
            let env = low_speed_test_env();
            let tmp = tempfile::tempdir().expect("tempdir");
            let (repo, _) = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
            let url = format!("http://127.0.0.1:{port}/x.git");
            let remote = Remote::from_url(&url).expect("remote url");
            let started = Instant::now();
            let err = remote
                .fetch(
                    &repo,
                    FetchOptions {
                        environment: env,
                        ..FetchOptions::default()
                    },
                    &mut NoProgress,
                    &CancelToken::never(),
                    Some(&DefaultHttpClientFactory::with_environment(
                        low_speed_test_env(),
                    )),
                )
                .expect_err("header fetch stall must fail");
            stop.store(true, Ordering::Relaxed);
            assert!(!err.is_cancelled(), "low-speed must not look like cancel");
            let msg = format!("{err}");
            assert!(
                msg.contains("HTTP transfer speed below http.lowSpeedLimit"),
                "expected low-speed transport error, got {msg}"
            );
            assert_low_speed_elapsed(started);
        },
    );
}

#[test]
fn connect_operation_cancel_watch_observes_mirrored_deadline() {
    let cancel = deadline_cancel_token(Instant::now() - Duration::from_secs(1));
    // `deadline_cancel_token` cancels on a spawned thread; wait so the flag is visible.
    for _ in 0..200 {
        if cancel.is_cancelled() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        cancel.is_cancelled(),
        "deadline_cancel_token should cancel immediately when the deadline is in the past"
    );
    let (watch, _guard) = HttpWaitGuard::bind(&cancel);
    let deadline = Instant::now() + TEST_HARD_TIMEOUT;
    while !cancel.is_cancelled() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    mirror_stack_cancel(&cancel, watch.as_ref());
    let arc = connect_cancel_arc(&cancel, watch);
    assert!(
        arc.is_cancelled(),
        "mirrored deadline must read cancelled through connect cancel arc"
    );
}
