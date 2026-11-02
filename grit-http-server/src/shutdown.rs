//! Graceful shutdown for the HTTP server: stop accepting connections, drain
//! in-flight requests, and optionally cap the wait.

use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;

/// How long [`crate::server::Server::run`] waits for in-flight work after shutdown starts.
#[derive(Debug)]
pub struct Shutdown {
    /// After the listener closes, keep serving responses until this long elapses.
    /// [`Duration::ZERO`] waits with no cap until every in-flight request finishes.
    pub grace: Duration,
    /// When set, this fires instead of the first process signal (for tests).
    pub(crate) trigger: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(30),
            trigger: None,
        }
    }
}

impl Shutdown {
    /// Shutdown settings with a custom grace period and normal signal handling.
    #[must_use]
    pub fn grace(grace: Duration) -> Self {
        Self {
            grace,
            trigger: None,
        }
    }

    /// Drive shutdown from `trigger` instead of OS signals (tests only).
    #[doc(hidden)]
    #[must_use]
    pub fn triggered(grace: Duration, trigger: tokio::sync::oneshot::Receiver<()>) -> Self {
        Self {
            grace,
            trigger: Some(trigger),
        }
    }
}

/// Wait for the first shutdown signal (Unix: SIGTERM, SIGINT, SIGHUP; Windows: Ctrl+C).
async fn first_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            return;
        };
        let Ok(mut int) = signal(SignalKind::interrupt()) else {
            return;
        };
        let Ok(mut hup) = signal(SignalKind::hangup()) else {
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
            _ = hup.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Serve `router` on `listener` until shutdown, calling `on_shutdown` when it begins.
///
/// # Errors
///
/// I/O errors from accepting or serving connections.
pub async fn serve_until_shutdown(
    listener: TcpListener,
    router: Router,
    shutdown: Shutdown,
    on_shutdown: impl FnOnce() + Send + 'static,
) -> std::io::Result<()> {
    let grace = shutdown.grace;
    let trigger = shutdown.trigger;
    let (drain_started_tx, drain_started_rx) = tokio::sync::oneshot::channel();

    let shutdown_started = async move {
        if let Some(trigger) = trigger {
            let _ = trigger.await;
        } else {
            first_signal().await;
        }
        eprintln!(
            "grit serve: closing listener; in-flight requests may finish (grace {})",
            format_grace(grace)
        );
        on_shutdown();
        let _ = drain_started_tx.send(());
        tokio::spawn(async {
            first_signal().await;
            eprintln!("grit serve: second shutdown signal; exiting now");
            std::process::exit(1);
        });
    };

    let serve = async {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_started)
            .await
    };

    if grace.is_zero() {
        serve.await
    } else {
        tokio::pin!(serve);
        tokio::select! {
            result = serve.as_mut() => result,
            _ = async {
                let _ = drain_started_rx.await;
                tokio::time::sleep(grace).await;
            } => {
                eprintln!(
                    "grit serve: grace period ({}) elapsed; closing remaining connections",
                    format_grace(grace)
                );
                Ok(())
            }
        }
    }
}

fn format_grace(grace: Duration) -> String {
    if grace.is_zero() {
        return "unlimited".to_owned();
    }
    let secs = grace.as_secs();
    if grace.subsec_nanos() == 0 && secs >= 60 && secs.is_multiple_of(60) {
        return format!("{}m", secs / 60);
    }
    if grace.subsec_nanos() == 0 {
        return format!("{secs}s");
    }
    format!("{grace:?}")
}

#[cfg(test)]
mod tests {
    use super::{serve_until_shutdown, Shutdown};
    use std::net::SocketAddr;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    use axum::routing::get;
    use axum::Router;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

    async fn slow_handler() -> &'static str {
        tokio::time::sleep(Duration::from_millis(200)).await;
        "ok"
    }

    async fn listen() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        (listener, addr)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_flight_request_finishes_inside_grace() {
        let (listener, addr) = listen().await;
        let app = Router::new().route("/slow", get(slow_handler));
        let (tx, rx) = oneshot::channel();
        let shutdown = Shutdown::triggered(Duration::from_secs(2), rx);
        let handle = tokio::spawn(serve_until_shutdown(listener, app, shutdown, || {}));

        let client = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut stream = TcpStream::connect(addr).await.expect("connect");
            let request =
                format!("GET /slow HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
            stream.write_all(request.as_bytes()).await.expect("write");
            let mut response = String::new();
            stream.read_to_string(&mut response).await.expect("read");
            response
        });

        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = tx.send(());
        let response = client.await.expect("client");
        assert!(
            response.contains("200"),
            "expected success inside grace: {response}"
        );
        handle.await.expect("server").expect("serve");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grace_timeout_ends_the_server() {
        let (listener, addr) = listen().await;
        let app = Router::new().route("/slow", get(slow_handler));
        let (tx, rx) = oneshot::channel();
        let grace = Duration::from_millis(80);
        let shutdown = Shutdown::triggered(grace, rx);
        let started = std::time::Instant::now();
        let handle = tokio::spawn(serve_until_shutdown(listener, app, shutdown, || {}));

        let _client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.expect("connect");
            let request =
                format!("GET /slow HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
            stream.write_all(request.as_bytes()).await.expect("write");
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = tx.send(());
        handle.await.expect("server").expect("serve");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "server should stop soon after grace, took {elapsed:?}"
        );
        assert!(
            elapsed >= grace.saturating_sub(Duration::from_millis(30)),
            "server should wait roughly the grace period, took {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_connections_are_not_accepted_after_shutdown_starts() {
        let (listener, addr) = listen().await;
        let started = Arc::new(AtomicUsize::new(0));
        let started_in_handler = started.clone();
        let app = Router::new().route(
            "/",
            get(move || {
                let started_in_handler = started_in_handler.clone();
                async move {
                    started_in_handler.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    "hold"
                }
            }),
        );
        let (tx, rx) = oneshot::channel();
        let shutdown = Shutdown::triggered(Duration::from_secs(5), rx);
        let handle = tokio::spawn(serve_until_shutdown(listener, app, shutdown, || {}));

        let _ = tx.send(());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let late = TcpStream::connect(addr).await;
        if let Ok(mut stream) = late {
            let request = format!("GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(request.as_bytes()).await;
            let mut response = String::new();
            let read = stream.read_to_string(&mut response).await;
            assert!(
                read.is_err() || response.is_empty() || !response.contains("200"),
                "late connect should not get a normal response: {response:?}"
            );
        }
        handle.await.expect("server").expect("serve");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nonzero_grace_does_not_stop_server_before_trigger() {
        let (listener, addr) = listen().await;
        let app = Router::new().route("/", get(|| async { "ok" }));
        let grace = Duration::from_millis(50);
        let (tx, rx) = oneshot::channel();
        let shutdown = Shutdown::triggered(grace, rx);
        let handle = tokio::spawn(serve_until_shutdown(listener, app, shutdown, || {}));

        tokio::time::sleep(grace + Duration::from_millis(100)).await;
        assert!(
            !handle.is_finished(),
            "server must keep running until shutdown is triggered"
        );

        let mut stream = TcpStream::connect(addr)
            .await
            .expect("connect while waiting");
        let request = format!("GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        assert!(
            response.contains("200"),
            "server should still answer: {response}"
        );

        let _ = tx.send(());
        handle.await.expect("server").expect("serve");
    }
}
