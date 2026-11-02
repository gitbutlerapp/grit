//! Drain unread HTTP request bodies before closing a connection.
//!
//! Reverse proxies relay the upstream status only if the backend finishes reading
//! (or half-closes cleanly) while the client is still sending a large body.
//! Early `401`/`404`/`413` responses must discard the remainder first.

use std::io::Read;

use axum::body::Body;
use axum::http::{header, HeaderValue, Response};
use futures_util::StreamExt;
use tokio::time::{timeout, Duration};

/// Maximum raw request bytes to discard after an early response.
pub const DISCARD_BYTE_CEILING: u64 = 1024 * 1024 * 1024;

/// How long to wait for the next body chunk while discarding.
pub const DISCARD_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Outcome of [`discard_request_body`].
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
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
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

/// Read `reader` until EOF on a blocking thread, ignoring errors.
pub fn drain_blocking_reader(mut reader: impl Read) {
    let mut buf = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
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
