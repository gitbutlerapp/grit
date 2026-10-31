//! Connection and smart-HTTP request limits for `grit serve`.
//!
//! [`ServeLimits`] configures header read deadlines, idle timeouts on request
//! bodies and upload-pack responses, and a cap on concurrent Git operations.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use futures_util::Stream;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tower::Service;

/// Timeouts and concurrency for Git smart HTTP on a running server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeLimits {
    /// How long to wait for the rest of the request headers after the first
    /// bytes (incomplete slowloris connections are dropped).
    pub header_timeout: Duration,
    /// Maximum idle time between body chunks while reading a POST body or
    /// waiting for the client to read an upload-pack response chunk.
    pub idle_timeout: Duration,
    /// Concurrent smart-HTTP operations (`info/refs`, upload-pack,
    /// receive-pack, resumable push apply). Further Git requests get `503`.
    pub max_git_requests: usize,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            header_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(60),
            max_git_requests: 128,
        }
    }
}

/// Parse a duration flag (`30s`, `2m`, `1h`, `1d`, or plain seconds).
///
/// # Errors
///
/// Describes values that are not a supported duration.
pub fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("`{value}` is not a duration like 30s, 2m or 1h"))?;
    let seconds = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(format!("`{value}` is not a duration like 30s, 2m or 1h")),
    };
    Ok(Duration::from_secs(number.saturating_mul(seconds)))
}

impl ServeLimits {
    /// A [`tokio::sync::Semaphore`] sized for [`Self::max_git_requests`].
    #[must_use]
    pub fn git_semaphore(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::new(tokio::sync::Semaphore::new(self.max_git_requests))
    }
}

/// Accept connections and serve `router` with HTTP/1 header read and HTTP/2
/// keep-alive limits from `limits`.
///
/// # Errors
///
/// Returns I/O errors from the listener or connection setup.
/// Accept connections and serve `router` with HTTP/1 header read limits until shutdown.
///
/// # Errors
///
/// Returns I/O errors from the listener or connection setup.
pub async fn serve_with_shutdown(
    router: Router,
    listener: TcpListener,
    limits: ServeLimits,
    shutdown: crate::shutdown::Shutdown,
    on_shutdown: impl FnOnce() + Send + 'static,
) -> io::Result<()> {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    let grace = shutdown.grace;
    let trigger = shutdown.trigger;
    let (drain_started_tx, drain_started_rx) = tokio::sync::oneshot::channel();
    let stop_accept = Arc::new(AtomicBool::new(false));
    let stop_flag = stop_accept.clone();

    tokio::spawn(async move {
        if let Some(trigger) = trigger {
            let _ = trigger.await;
        } else {
            crate::shutdown::wait_first_signal().await;
        }
        eprintln!(
            "grit serve: closing listener; in-flight requests may finish (grace {})",
            crate::shutdown::format_grace_for_log(grace)
        );
        stop_flag.store(true, Ordering::SeqCst);
        on_shutdown();
        let _ = drain_started_tx.send(());
        tokio::spawn(async {
            crate::shutdown::wait_first_signal().await;
            eprintln!("grit serve: second shutdown signal; exiting now");
            std::process::exit(1);
        });
    });

    let make_service = router.into_make_service();
    let header_timeout = limits.header_timeout;
    while !stop_accept.load(Ordering::SeqCst) {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) if stop_accept.load(Ordering::SeqCst) => break,
            Err(e) => return Err(e),
        };
        let io = TokioIo::new(stream);
        let mut make_service = make_service.clone();
        tokio::spawn(async move {
            let service = match make_service.call(()).await {
                Ok(service) => service,
                Err(_) => return,
            };
            let hyper_service = TowerToHyperService::new(service);
            let mut builder = Builder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(Some(header_timeout));
            builder
                .http2()
                .keep_alive_interval(Some(Duration::from_secs(30)))
                .keep_alive_timeout(Duration::from_secs(10));
            let _ = builder
                .serve_connection_with_upgrades(io, hyper_service)
                .await;
        });
    }

    if grace.is_zero() {
        let _ = drain_started_rx.await;
        return Ok(());
    }
    let _ = drain_started_rx.await;
    tokio::time::sleep(grace).await;
    eprintln!(
        "grit serve: grace period ({}) elapsed; closing remaining connections",
        crate::shutdown::format_grace_for_log(grace)
    );
    Ok(())
}

pub async fn serve(router: Router, listener: TcpListener, limits: ServeLimits) -> io::Result<()> {
    let make_service = router.into_make_service();
    let header_timeout = limits.header_timeout;
    loop {
        let (stream, _addr) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let mut make_service = make_service.clone();
        tokio::spawn(async move {
            let service = match make_service.call(()).await {
                Ok(service) => service,
                Err(_) => return,
            };
            let hyper_service = TowerToHyperService::new(service);
            let mut builder = Builder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(Some(header_timeout));
            builder
                .http2()
                .keep_alive_interval(Some(Duration::from_secs(30)))
                .keep_alive_timeout(Duration::from_secs(10));
            let _ = builder
                .serve_connection_with_upgrades(io, hyper_service)
                .await;
        });
    }
}

/// Request body or response channel sat idle longer than allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyIdleTimeout;

impl std::fmt::Display for BodyIdleTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request idle timeout")
    }
}

impl std::error::Error for BodyIdleTimeout {}

/// Whether `err` is a [`BodyIdleTimeout`].
#[must_use]
pub fn io_error_is_idle_timeout(err: &io::Error) -> bool {
    err.get_ref()
        .is_some_and(|inner| inner.is::<BodyIdleTimeout>())
}

/// Idle timeout between chunks of an HTTP body byte stream.
pub struct IdleTimeoutStream<S> {
    inner: S,
    idle: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
    armed: bool,
}

impl<S, E> IdleTimeoutStream<S>
where
    S: Stream<Item = Result<axum::body::Bytes, E>> + Unpin,
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    /// Wrap `inner` so each gap between items longer than `idle` fails the stream.
    #[must_use]
    pub fn new(inner: S, idle: Duration) -> Self {
        Self {
            inner,
            idle,
            sleep: Box::pin(tokio::time::sleep(Duration::ZERO)),
            armed: false,
        }
    }

    fn arm_deadline(&mut self) {
        self.sleep
            .as_mut()
            .reset(tokio::time::Instant::now() + self.idle);
        self.armed = true;
    }

    fn idle_error() -> io::Error {
        io::Error::new(io::ErrorKind::TimedOut, BodyIdleTimeout)
    }
}

impl<S, E> Stream for IdleTimeoutStream<S>
where
    S: Stream<Item = Result<axum::body::Bytes, E>> + Unpin,
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Item = Result<axum::body::Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if !this.armed {
            this.arm_deadline();
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.arm_deadline();
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(io::Error::other(e.into())))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Some(Err(Self::idle_error()))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use futures_util::StreamExt as _;

    #[tokio::test]
    async fn idle_timeout_fires_when_chunks_stop() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let mut wrapped = IdleTimeoutStream::new(stream, Duration::from_millis(50));
        assert!(tx.send(Ok(Bytes::from_static(b"a"))).await.is_ok());
        let first = wrapped.next().await.expect("chunk").expect("ok chunk");
        assert_eq!(first, Bytes::from_static(b"a"));
        let err = wrapped
            .next()
            .await
            .expect("second read")
            .expect_err("must time out");
        assert!(io_error_is_idle_timeout(&err));
        drop(tx);
    }

    #[tokio::test]
    async fn steady_chunks_do_not_time_out() {
        let chunks: Vec<Result<Bytes, std::convert::Infallible>> =
            vec![Ok(Bytes::from_static(b"1")), Ok(Bytes::from_static(b"2"))];
        let stream = futures_util::stream::iter(chunks);
        let mut wrapped = IdleTimeoutStream::new(stream, Duration::from_millis(200));
        let one = wrapped.next().await.expect("one").expect("ok");
        assert_eq!(one, Bytes::from_static(b"1"));
        let two = wrapped.next().await.expect("two").expect("ok");
        assert_eq!(two, Bytes::from_static(b"2"));
        assert!(wrapped.next().await.is_none());
    }
}
