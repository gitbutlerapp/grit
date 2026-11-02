//! Drain unread HTTP request bodies before closing a connection.
//!
//! Reverse proxies relay the upstream status only if the backend finishes reading
//! (or half-closes cleanly) while the client is still sending a large body.
//! Early `401`/`404`/`413` responses must discard the remainder first.

use std::io::{self, Cursor, Read};
use std::sync::mpsc::{Receiver, RecvError, SyncSender};

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderValue, Response};
use futures_util::StreamExt;
use tokio::sync::oneshot;
use tokio::time::{timeout, Duration};

/// Maximum raw request bytes to discard after an early response.
pub const DISCARD_BYTE_CEILING: u64 = 1024 * 1024 * 1024;

/// How long to wait for the next body chunk while discarding.
pub const DISCARD_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Outcome of [`discard_request_body`] and other discard helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscardOutcome {
    /// The body hit [`DISCARD_BYTE_CEILING`] or [`DISCARD_IDLE_TIMEOUT`].
    pub force_connection_close: bool,
}

/// Stream `body` to EOF (or until limits), dropping each chunk.
///
/// Returns whether the caller should set `Connection: close` on the response.
#[must_use]
pub async fn discard_request_body(body: Body) -> DiscardOutcome {
    discard_data_stream(&mut body.into_data_stream(), 0).await
}

/// Discard the remainder of a request body stream under the shared limits.
pub async fn discard_data_stream(
    stream: &mut (impl futures_util::Stream<Item = Result<Bytes, axum::Error>> + Unpin),
    mut total: u64,
) -> DiscardOutcome {
    loop {
        match timeout(DISCARD_IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                total = total.saturating_add(chunk.len() as u64);
                if total > DISCARD_BYTE_CEILING {
                    return DiscardOutcome {
                        force_connection_close: true,
                    };
                }
            }
            Ok(Some(Err(_))) | Ok(None) => {
                return DiscardOutcome {
                    force_connection_close: false,
                };
            }
            Err(_) => {
                return DiscardOutcome {
                    force_connection_close: true,
                };
            }
        }
    }
}

/// Attach `Connection: close` when discarding hit a limit.
#[must_use]
pub fn apply_discard_outcome<T>(mut response: Response<T>, outcome: DiscardOutcome) -> Response<T> {
    if outcome.force_connection_close {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

/// Blocking [`Read`] end of [`open_body_bridge`].
pub struct BodyBridgeRead {
    rx: Receiver<Result<Vec<u8>, io::Error>>,
    pending: Cursor<Vec<u8>>,
}

impl Read for BodyBridgeRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pending.position() < self.pending.get_ref().len() as u64 {
                return self.pending.read(buf);
            }
            self.pending.set_position(0);
            self.pending.get_mut().clear();
            match self.rx.recv() {
                Ok(Ok(chunk)) => {
                    if chunk.is_empty() {
                        return Ok(0);
                    }
                    *self.pending.get_mut() = chunk;
                }
                Ok(Err(e)) => return Err(e),
                Err(RecvError) => return Ok(0),
            }
        }
    }
}

/// Async pump task for [`open_body_bridge`]; call [`Self::finish`] when the
/// blocking consumer is done.
pub struct BodyBridge {
    join: tokio::task::JoinHandle<DiscardOutcome>,
    force_discard: Option<oneshot::Sender<()>>,
}

impl BodyBridge {
    /// Wait for the pump to finish after the blocking reader is dropped.
    ///
    /// When `force_discard` is true, stop forwarding to the decoder immediately
    /// and discard the rest of the stream under [`DISCARD_IDLE_TIMEOUT`] /
    /// [`DISCARD_BYTE_CEILING`].
    pub async fn finish(mut self, force_discard: bool) -> DiscardOutcome {
        if force_discard {
            if let Some(tx) = self.force_discard.take() {
                let _ = tx.send(());
            }
        }
        self.join.await.unwrap_or(DiscardOutcome {
            force_connection_close: true,
        })
    }
}

/// Split a request body into a blocking reader and a bounded async pump.
#[must_use]
pub fn open_body_bridge(body: Body) -> (BodyBridgeRead, BodyBridge) {
    let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, io::Error>>(8);
    let (force_tx, force_rx) = oneshot::channel();
    let stream = body.into_data_stream();
    let join = tokio::spawn(body_pump(stream, chunk_tx, force_rx));
    (
        BodyBridgeRead {
            rx: chunk_rx,
            pending: Cursor::new(Vec::new()),
        },
        BodyBridge {
            join,
            force_discard: Some(force_tx),
        },
    )
}

async fn body_pump(
    mut stream: impl futures_util::Stream<Item = Result<Bytes, axum::Error>> + Unpin,
    chunk_tx: SyncSender<Result<Vec<u8>, io::Error>>,
    mut force_discard: oneshot::Receiver<()>,
) -> DiscardOutcome {
    loop {
        tokio::select! {
            _ = &mut force_discard => {
                return discard_data_stream(&mut stream, 0).await;
            }
            item = stream.next() => {
                match item {
                    Some(Ok(chunk)) => {
                        if chunk_tx.send(Ok(chunk.to_vec())).is_err() {
                            return discard_data_stream(&mut stream, chunk.len() as u64).await;
                        }
                    }
                    Some(Err(_)) => {
                        return DiscardOutcome {
                            force_connection_close: false,
                        };
                    }
                    None => {
                        return DiscardOutcome {
                            force_connection_close: false,
                        };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    #[tokio::test]
    async fn discard_empty_body() {
        let outcome = discard_request_body(Body::empty()).await;
        assert!(!outcome.force_connection_close);
    }

    #[tokio::test]
    async fn discard_small_body() {
        let body = Body::from_stream(stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"hello"))
        }));
        let outcome = discard_request_body(body).await;
        assert!(!outcome.force_connection_close);
    }
}
