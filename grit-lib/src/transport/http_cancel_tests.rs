//! HTTP transport cancellation tests (issue #2469).

#![cfg(all(test, feature = "http-ureq"))]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::clone::{clone, CloneError, CloneOptions};
use crate::environment::Environment;
use crate::error::Error;
use crate::pkt_line::write_line_to_vec;
use crate::progress::{
    connect_cancel_arc, mirror_stack_cancel, Cancel, FlagCancel, HttpWaitGuard, NeverCancel,
};
use crate::remote::fetch::NoProgress;
use crate::remote::transfer::FetchOptions;
use crate::remote::{DefaultHttpClientFactory, Remote};
use crate::repo::{InitOptions, Repository};
use crate::transport::http::http_client_arc;
use crate::transport::http::ureq_client::UreqHttpClient;
use crate::transport::http::SmartHttpTransport;
use crate::transport::{ConnectOptions, Connection, Service, Transport};

const TEST_HARD_TIMEOUT: Duration = Duration::from_secs(5);

fn run_with_hard_timeout<T: Send + 'static>(
    label: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(TEST_HARD_TIMEOUT)
        .unwrap_or_else(|_| panic!("{label} did not finish within {TEST_HARD_TIMEOUT:?}"))
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

        let cancel = FlagCancel::new();
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
    let cancel = DeadlineCancel(Instant::now() + Duration::from_millis(50));
    let watch = std::sync::atomic::AtomicBool::new(false);
    thread::sleep(Duration::from_millis(80));
    mirror_stack_cancel(&cancel, &watch);
    assert!(
        watch.load(Ordering::Relaxed),
        "deadline cancel must mirror into stack watch"
    );
}

/// Deadline-based [`Cancel`] without [`Cancel::shared_watch`].
struct DeadlineCancel(Instant);

impl Cancel for DeadlineCancel {
    fn is_cancelled(&self) -> bool {
        Instant::now() >= self.0
    }
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

            let cancel = DeadlineCancel(Instant::now() + Duration::from_millis(300));
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

            let cancel = DeadlineCancel(Instant::now() + Duration::from_millis(300));
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
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

            let cancel = DeadlineCancel(Instant::now() + Duration::from_millis(300));
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
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

            let cancel = DeadlineCancel(Instant::now() + Duration::from_millis(300));
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

        let cancel = FlagCancel::new();
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
            &NeverCancel,
            Some(&factory),
        )
        .expect_err("low-speed stall must not succeed");
        stop.store(true, Ordering::Relaxed);
        drop(listener);
        assert!(
            matches!(err, CloneError::Remote(_) | CloneError::Library(_)),
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
fn connect_operation_cancel_watch_observes_mirrored_deadline() {
    let cancel = DeadlineCancel(Instant::now() - Duration::from_secs(1));
    let (watch, _guard) = HttpWaitGuard::bind(&cancel);
    mirror_stack_cancel(&cancel, watch.as_ref());
    let arc = connect_cancel_arc(&cancel, watch);
    assert!(
        arc.is_cancelled(),
        "mirrored deadline must read cancelled through connect cancel arc"
    );
}
