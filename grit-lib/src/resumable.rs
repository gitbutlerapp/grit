//! Transfers that survive a dropped connection: downloads resumed with HTTP
//! `Range` requests, and uploads over the [TUS] resumable-upload protocol.
//!
//! Clone uses [`download_resumable`] for the bundle a server advertises with
//! `bundle-uri` ([`crate::bundle_uri`]); push uses [`TusUpload`] to send a large
//! bundle to the endpoint a server advertises with `pushBundle`
//! ([`crate::push`]). Both keep their progress on disk, so a transfer that is
//! interrupted (even by the process exiting) picks up where it stopped the next
//! time it runs, and both retry a dropped connection a few times before giving
//! up.
//!
//! Requests go through [`HttpClient::send`]; clients that do not implement it
//! cannot make these transfers. Progress is reported both as the
//! [`ProgressPhase::TransferringBundle`] phase and, in more detail, as
//! [`ResumableEvent`]s.
//!
//! [TUS]: https://tus.io/protocols/resumable-upload

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fetch::Progress;
use crate::progress::{ProgressPhase, ProgressUnit};
use crate::transport::http::{HttpClient, HttpMethod, HttpRequest, HttpResponse};

/// TUS protocol version this client and `grit serve` speak.
pub const TUS_VERSION: &str = "1.0.0";

/// Content type of a TUS `PATCH` body.
pub const TUS_PATCH_CONTENT_TYPE: &str = "application/offset+octet-stream";

/// Bytes sent per TUS `PATCH` request.
const PATCH_CHUNK: usize = 4 * 1024 * 1024;

/// Bytes read from a download body between progress reports.
const READ_CHUNK: usize = 64 * 1024;

/// Times a dropped connection is retried before the transfer gives up.
const MAX_ATTEMPTS: u32 = 5;

/// Which way a resumable transfer moves data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    /// Downloading a bundle from the server.
    Download,
    /// Uploading a bundle to the server.
    Upload,
}

/// Progress of a resumable transfer, reported through
/// [`Progress::resumable_transfer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumableEvent {
    /// The transfer began. `resumed_from` is how many bytes an earlier attempt
    /// had already moved (zero for a fresh transfer).
    Started {
        /// Download or upload.
        direction: TransferDirection,
        /// The URL being transferred.
        url: String,
        /// Total bytes, when known.
        total: Option<u64>,
        /// Bytes already in place before this run.
        resumed_from: u64,
    },
    /// More bytes arrived or were accepted.
    Advanced {
        /// Bytes in place so far, including `resumed_from`.
        done: u64,
        /// Total bytes, when known.
        total: Option<u64>,
    },
    /// The connection dropped; the transfer is picking up again at `done`.
    Reconnecting {
        /// Bytes in place when the connection dropped.
        done: u64,
        /// Which retry this is, starting at 1.
        attempt: u32,
    },
    /// Every byte is in place.
    Finished {
        /// Total bytes transferred.
        done: u64,
        /// Bytes that were already in place before this run.
        resumed_from: u64,
    },
}

/// Why a resumable transfer failed.
#[derive(Debug, thiserror::Error)]
pub enum ResumableError {
    /// The server answered with a status the transfer cannot continue from.
    #[error("HTTP {status} from {url}")]
    Status {
        /// The request URL.
        url: String,
        /// The response status.
        status: u16,
    },
    /// The server's answer did not follow the protocol.
    #[error("unexpected response from {url}: {detail}")]
    Protocol {
        /// The request URL.
        url: String,
        /// What was wrong.
        detail: String,
    },
    /// The connection kept dropping.
    #[error("the transfer of {url} kept failing after {attempts} attempts: {last}")]
    Exhausted {
        /// The URL being transferred.
        url: String,
        /// Attempts made.
        attempts: u32,
        /// The last failure.
        last: String,
    },
    /// The HTTP client failed (transport or authentication).
    #[error(transparent)]
    Http(Box<crate::error::Error>),
    /// Reading or writing the local file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<crate::error::Error> for ResumableError {
    fn from(e: crate::error::Error) -> Self {
        Self::Http(Box::new(e))
    }
}

/// Result alias for resumable transfers.
pub type Result<T> = std::result::Result<T, ResumableError>;

/// What [`download_resumable`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadOutcome {
    /// Size of the downloaded file.
    pub bytes: u64,
    /// Bytes that were already on disk from an earlier, interrupted run.
    pub resumed_from: u64,
}

/// What is known about a partial download, kept next to it as JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct DownloadState {
    /// The URL the bytes came from; another URL starts over.
    url: String,
    /// The server's `ETag` (or `Last-Modified`), sent back as `If-Range` so a
    /// changed file is downloaded afresh instead of spliced.
    validator: Option<String>,
    /// Total size, once the server said.
    total: Option<u64>,
    /// Whether every byte is in place (the file is at its final path).
    complete: bool,
}

/// The files a download to `dest` keeps: the bytes so far and their state.
fn download_paths(dest: &Path) -> (PathBuf, PathBuf) {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    (
        dest.with_file_name(format!("{name}.part")),
        dest.with_file_name(format!("{name}.json")),
    )
}

fn load_state(path: &Path) -> Option<DownloadState> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn save_state(path: &Path, state: &DownloadState) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(state).map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Remove `dest` and everything [`download_resumable`] keeps for it.
///
/// # Errors
///
/// A removal failure other than the file not existing.
pub fn discard_download(dest: &Path) -> std::io::Result<()> {
    let (part, state) = download_paths(dest);
    for path in [dest, part.as_path(), state.as_path()] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Download `url` to `dest`, continuing an earlier interrupted download.
///
/// Bytes arrive in `<dest>.part`, with what is known about them in
/// `<dest>.json`; the file is renamed to `dest` once complete. When a
/// previous run left a partial file for the same URL, only the rest is asked
/// for (`Range: bytes=<n>-`, with `If-Range` so a file that changed on the
/// server starts over). A connection that drops mid-download is retried a few
/// times from where it stopped. When `dest` is already complete for `url`,
/// nothing is downloaded.
///
/// # Parameters
///
/// - `client`: sends the requests ([`HttpClient::send`]).
/// - `url`: what to download.
/// - `dest`: where the finished file goes; its directory must exist.
/// - `progress`: receives [`ResumableEvent`]s.
///
/// # Errors
///
/// [`ResumableError::Status`] when the server refuses the download,
/// [`ResumableError::Exhausted`] when the connection keeps dropping, and I/O
/// errors from the local files. The partial file is kept so a later call can
/// resume.
pub fn download_resumable(
    client: &dyn HttpClient,
    url: &str,
    dest: &Path,
    progress: &mut dyn Progress,
) -> Result<DownloadOutcome> {
    let (part, state_path) = download_paths(dest);
    let mut state = load_state(&state_path).unwrap_or_default();
    if state.url != url {
        discard_download(dest)?;
        state = DownloadState {
            url: url.to_owned(),
            ..DownloadState::default()
        };
    }
    if state.complete {
        if let (Some(total), Ok(meta)) = (state.total, std::fs::metadata(dest)) {
            if meta.len() == total {
                return Ok(DownloadOutcome {
                    bytes: total,
                    resumed_from: total,
                });
            }
        }
        discard_download(dest)?;
        state = DownloadState {
            url: url.to_owned(),
            ..DownloadState::default()
        };
    }

    let resumed_from = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut started = false;
    let mut attempt = 0;
    let mut last_failure = String::new();
    loop {
        if attempt > 0 {
            let done = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            if attempt >= MAX_ATTEMPTS {
                return Err(ResumableError::Exhausted {
                    url: url.to_owned(),
                    attempts: attempt,
                    last: last_failure,
                });
            }
            progress.resumable_transfer(&ResumableEvent::Reconnecting { done, attempt });
        }
        attempt += 1;
        match download_attempt(
            client,
            url,
            &part,
            &mut state,
            &state_path,
            progress,
            |p, s| {
                if !started {
                    started = true;
                    p.start_with_unit(
                        ProgressPhase::TransferringBundle,
                        ProgressUnit::Bytes,
                        s.total,
                    );
                    p.set(s.resumed_from);
                    p.resumable_transfer(&ResumableEvent::Started {
                        direction: TransferDirection::Download,
                        url: url.to_owned(),
                        total: s.total,
                        resumed_from: s.resumed_from,
                    });
                }
            },
        ) {
            Ok(AttemptOutcome::Complete(bytes)) => {
                std::fs::rename(&part, dest)?;
                state.complete = true;
                state.total = Some(bytes);
                save_state(&state_path, &state)?;
                progress.set(bytes);
                progress.finish();
                progress.resumable_transfer(&ResumableEvent::Finished {
                    done: bytes,
                    resumed_from: resumed_from.min(bytes),
                });
                return Ok(DownloadOutcome {
                    bytes,
                    resumed_from: resumed_from.min(bytes),
                });
            }
            Ok(AttemptOutcome::Dropped(why)) => last_failure = why,
            Err(e) => return Err(e),
        }
    }
}

/// Where a download attempt starts, reported once when the first one answers.
struct AttemptStart {
    total: Option<u64>,
    resumed_from: u64,
}

enum AttemptOutcome {
    /// Every byte is in the partial file; the value is its size.
    Complete(u64),
    /// The connection dropped before the end.
    Dropped(String),
}

/// One request of a download: ask for the bytes not yet on disk and append
/// them to `part`.
#[allow(clippy::too_many_arguments)]
fn download_attempt(
    client: &dyn HttpClient,
    url: &str,
    part: &Path,
    state: &mut DownloadState,
    state_path: &Path,
    progress: &mut dyn Progress,
    mut on_start: impl FnMut(&mut dyn Progress, &AttemptStart),
) -> Result<AttemptOutcome> {
    let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    let mut request = HttpRequest::new(HttpMethod::Get, url).long_running();
    if have > 0 {
        request = request.header("Range", format!("bytes={have}-"));
        if let Some(validator) = &state.validator {
            request = request.header("If-Range", validator.clone());
        }
    }
    let response = match client.send(&request) {
        Ok(r) => r,
        Err(e) => return Ok(AttemptOutcome::Dropped(e.to_string())),
    };
    let (mut offset, total) =
        match response.status {
            206 if have > 0 => {
                let (start, total) = parse_content_range(response.header("content-range"))
                    .ok_or_else(|| ResumableError::Protocol {
                        url: url.to_owned(),
                        detail: "206 without a usable Content-Range".to_owned(),
                    })?;
                if start != have {
                    return Err(ResumableError::Protocol {
                        url: url.to_owned(),
                        detail: format!("asked for bytes from {have}, got bytes from {start}"),
                    });
                }
                (have, total)
            }
            200 | 206 => {
                // A full answer: the server ignored the range or the file changed.
                let total = response
                    .header("content-length")
                    .and_then(|v| v.trim().parse().ok());
                (0, total)
            }
            416 if have > 0 && state.total == Some(have) => {
                on_start(
                    progress,
                    &AttemptStart {
                        total: state.total,
                        resumed_from: have,
                    },
                );
                return Ok(AttemptOutcome::Complete(have));
            }
            416 => {
                // The partial file is longer than the server's: start over.
                File::create(part)?;
                return Ok(AttemptOutcome::Dropped(
                    "the server's file is shorter than the partial download".to_owned(),
                ));
            }
            status => {
                return Err(ResumableError::Status {
                    url: url.to_owned(),
                    status,
                })
            }
        };
    state.total = total;
    state.validator = validator(&response);
    save_state(state_path, state)?;
    on_start(
        progress,
        &AttemptStart {
            total,
            resumed_from: offset,
        },
    );

    let mut file = if offset == 0 {
        File::create(part)?
    } else {
        OpenOptions::new().append(true).open(part)?
    };
    let mut body = response.body;
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                file.flush()?;
                return Ok(AttemptOutcome::Dropped(e.to_string()));
            }
        };
        file.write_all(&buf[..n])?;
        offset += n as u64;
        progress.set(offset);
        progress.resumable_transfer(&ResumableEvent::Advanced {
            done: offset,
            total,
        });
    }
    file.flush()?;
    file.sync_all()?;
    match total {
        Some(total) if offset < total => Ok(AttemptOutcome::Dropped(format!(
            "the connection closed after {offset} of {total} bytes"
        ))),
        _ => Ok(AttemptOutcome::Complete(offset)),
    }
}

/// The value to send back as `If-Range`: a strong `ETag`, else `Last-Modified`.
fn validator(response: &HttpResponse) -> Option<String> {
    response
        .header("etag")
        .filter(|v| !v.starts_with("W/"))
        .or_else(|| response.header("last-modified"))
        .map(str::to_owned)
}

/// Parse `bytes <start>-<end>/<total>` into the start and (known) total.
fn parse_content_range(value: Option<&str>) -> Option<(u64, Option<u64>)> {
    let rest = value?.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, _end) = range.split_once('-')?;
    let start = start.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((start, total))
}

/// An upload to a TUS endpoint.
///
/// [`TusUpload::create`] asks the endpoint for a new upload of a known length
/// and returns its URL; [`TusUpload::send_file`] sends a file to it, starting
/// at whatever offset the server already has, so calling it again after an
/// interruption (with the same upload URL) only sends the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TusUpload {
    /// Absolute URL of the upload.
    pub url: String,
    /// Total bytes of the upload.
    pub length: u64,
}

impl TusUpload {
    /// Create an upload of `length` bytes at `endpoint` (TUS creation
    /// extension).
    ///
    /// # Errors
    ///
    /// [`ResumableError::Status`] when the server refuses, and
    /// [`ResumableError::Protocol`] when it answers without a `Location`.
    pub fn create(client: &dyn HttpClient, endpoint: &str, length: u64) -> Result<Self> {
        let request = HttpRequest::new(HttpMethod::Post, endpoint)
            .header("Tus-Resumable", TUS_VERSION)
            .header("Upload-Length", length.to_string());
        let response = client.send(&request)?;
        if response.status != 201 {
            return Err(ResumableError::Status {
                url: endpoint.to_owned(),
                status: response.status,
            });
        }
        let location = response
            .header("location")
            .ok_or_else(|| ResumableError::Protocol {
                url: endpoint.to_owned(),
                detail: "no Location for the new upload".to_owned(),
            })?;
        let url = resolve_url(endpoint, location).ok_or_else(|| ResumableError::Protocol {
            url: endpoint.to_owned(),
            detail: format!("bad Location {location:?}"),
        })?;
        Ok(Self { url, length })
    }

    /// How many bytes the server has, or `None` when it no longer knows the
    /// upload (expired or already used).
    ///
    /// # Errors
    ///
    /// [`ResumableError::Status`] for other refusals, and
    /// [`ResumableError::Protocol`] for an answer without `Upload-Offset`.
    pub fn offset(&self, client: &dyn HttpClient) -> Result<Option<u64>> {
        let request =
            HttpRequest::new(HttpMethod::Head, &self.url).header("Tus-Resumable", TUS_VERSION);
        let response = client.send(&request)?;
        match response.status {
            404 | 410 => Ok(None),
            s if (200..300).contains(&s) => upload_offset(&response, &self.url).map(Some),
            status => Err(ResumableError::Status {
                url: self.url.clone(),
                status,
            }),
        }
    }

    /// Send `file` (exactly [`Self::length`] bytes) to the upload, starting at
    /// the server's offset and resynchronizing after each failed request.
    ///
    /// # Errors
    ///
    /// [`ResumableError::Status`] when the server no longer knows the upload
    /// (`404`/`410`) or refuses it, [`ResumableError::Exhausted`] when requests
    /// keep failing, and I/O errors reading `file`.
    pub fn send_file(
        &self,
        client: &dyn HttpClient,
        file: &Path,
        progress: &mut dyn Progress,
    ) -> Result<()> {
        let gone = |status| ResumableError::Status {
            url: self.url.clone(),
            status,
        };
        let mut offset = self.offset(client)?.ok_or_else(|| gone(404))?;
        let resumed_from = offset;
        progress.start_with_unit(
            ProgressPhase::TransferringBundle,
            ProgressUnit::Bytes,
            Some(self.length),
        );
        progress.set(offset);
        progress.resumable_transfer(&ResumableEvent::Started {
            direction: TransferDirection::Upload,
            url: self.url.clone(),
            total: Some(self.length),
            resumed_from,
        });
        let mut input = File::open(file)?;
        let mut buf = vec![0u8; PATCH_CHUNK];
        let mut failures = 0;
        while offset < self.length {
            let n = usize::try_from(self.length - offset)
                .map_or(PATCH_CHUNK, |left| left.min(PATCH_CHUNK));
            input.seek(SeekFrom::Start(offset))?;
            input.read_exact(&mut buf[..n])?;
            let request = HttpRequest::new(HttpMethod::Patch, &self.url)
                .header("Tus-Resumable", TUS_VERSION)
                .header("Upload-Offset", offset.to_string())
                .header("Content-Type", TUS_PATCH_CONTENT_TYPE)
                .body(&buf[..n])
                .long_running();
            let failure = match client.send(&request) {
                Ok(r) if r.status == 204 || r.status == 200 => {
                    offset = upload_offset(&r, &self.url)?;
                    failures = 0;
                    progress.set(offset);
                    progress.resumable_transfer(&ResumableEvent::Advanced {
                        done: offset,
                        total: Some(self.length),
                    });
                    continue;
                }
                Ok(r) if r.status == 404 || r.status == 410 => return Err(gone(r.status)),
                Ok(r) if r.status == 409 => format!("offset {offset} was out of date"),
                Ok(r) if (400..500).contains(&r.status) => return Err(gone(r.status)),
                Ok(r) => format!("HTTP {}", r.status),
                Err(e) => e.to_string(),
            };
            failures += 1;
            if failures >= MAX_ATTEMPTS {
                return Err(ResumableError::Exhausted {
                    url: self.url.clone(),
                    attempts: failures,
                    last: failure,
                });
            }
            progress.resumable_transfer(&ResumableEvent::Reconnecting {
                done: offset,
                attempt: failures,
            });
            // Ask the server where it got to; it may have kept part of the chunk.
            if let Ok(Some(server)) = self.offset(client) {
                offset = server;
            }
        }
        progress.set(self.length);
        progress.finish();
        progress.resumable_transfer(&ResumableEvent::Finished {
            done: self.length,
            resumed_from,
        });
        Ok(())
    }
}

fn upload_offset(response: &HttpResponse, url: &str) -> Result<u64> {
    response
        .header("upload-offset")
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| ResumableError::Protocol {
            url: url.to_owned(),
            detail: "no Upload-Offset".to_owned(),
        })
}

/// Resolve `reference` (absolute, or relative to `base`) to an absolute URL.
#[must_use]
pub fn resolve_url(base: &str, reference: &str) -> Option<String> {
    if let Ok(absolute) = url::Url::parse(reference) {
        return Some(absolute.to_string());
    }
    url::Url::parse(base)
        .ok()?
        .join(reference)
        .ok()
        .map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::NoProgress;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Serves one file from memory, honoring `Range`, and can cut a body short.
    struct FileServer {
        data: Vec<u8>,
        etag: String,
        /// Bytes to send before dropping, one entry per GET (then full bodies).
        cuts: Mutex<VecDeque<usize>>,
        ranges: Mutex<Vec<Option<String>>>,
    }

    struct Cut {
        data: std::io::Cursor<Vec<u8>>,
        fail_after: bool,
    }

    impl Read for Cut {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.data.read(buf)?;
            if n == 0 && self.fail_after {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset",
                ));
            }
            Ok(n)
        }
    }

    impl HttpClient for FileServer {
        fn get(&self, _: &str, _: Option<&str>) -> crate::error::Result<Vec<u8>> {
            unreachable!()
        }
        fn post(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: Option<&str>,
        ) -> crate::error::Result<Vec<u8>> {
            unreachable!()
        }
        fn send(&self, request: &HttpRequest<'_>) -> crate::error::Result<HttpResponse> {
            let range = request
                .headers
                .iter()
                .find(|(n, _)| *n == "Range")
                .map(|(_, v)| v.clone());
            let if_range = request
                .headers
                .iter()
                .find(|(n, _)| *n == "If-Range")
                .map(|(_, v)| v.clone());
            self.ranges
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(range.clone());
            let start = range
                .as_deref()
                .and_then(|r| r.strip_prefix("bytes="))
                .and_then(|r| r.strip_suffix('-'))
                .and_then(|r| r.parse::<usize>().ok())
                .filter(|_| if_range.as_deref().is_none_or(|v| v == self.etag));
            let total = self.data.len();
            let (status, mut headers, body) = match start {
                Some(s) if s >= total => (416, vec![], vec![]),
                Some(s) => (
                    206,
                    vec![(
                        "content-range".to_owned(),
                        format!("bytes {s}-{}/{total}", total - 1),
                    )],
                    self.data[s..].to_vec(),
                ),
                None => (
                    200,
                    vec![("content-length".to_owned(), total.to_string())],
                    self.data.clone(),
                ),
            };
            headers.push(("etag".to_owned(), self.etag.clone()));
            let cut = self
                .cuts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            let (body, fail_after) = match cut {
                Some(n) if n < body.len() => (body[..n].to_vec(), true),
                _ => (body, false),
            };
            Ok(HttpResponse {
                status,
                headers,
                body: Box::new(Cut {
                    data: std::io::Cursor::new(body),
                    fail_after,
                }),
            })
        }
    }

    #[derive(Default)]
    struct Events(Vec<ResumableEvent>);
    impl crate::progress::ProgressSink for Events {}
    impl Progress for Events {
        fn resumable_transfer(&mut self, event: &ResumableEvent) {
            if !matches!(event, ResumableEvent::Advanced { .. }) {
                self.0.push(event.clone());
            }
        }
    }

    fn server(cuts: &[usize]) -> FileServer {
        FileServer {
            data: (0..200_000u32).map(|i| (i % 251) as u8).collect(),
            etag: "\"v1\"".to_owned(),
            cuts: Mutex::new(cuts.iter().copied().collect()),
            ranges: Mutex::new(Vec::new()),
        }
    }

    fn tempdir() -> tempfile::TempDir {
        match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => panic!("tempdir: {e}"),
        }
    }

    #[test]
    fn dropped_downloads_resume_in_the_same_run() {
        let dir = tempdir();
        let dest = dir.path().join("clone.bundle");
        let srv = server(&[70_000, 10_000]);
        let mut events = Events::default();
        let outcome = download_resumable(&srv, "http://h/b", &dest, &mut events);
        assert_eq!(
            outcome.ok(),
            Some(DownloadOutcome {
                bytes: 200_000,
                resumed_from: 0
            })
        );
        assert_eq!(std::fs::read(&dest).ok(), Some(srv.data.clone()));
        let ranges = srv.ranges.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            ranges,
            [
                None,
                Some("bytes=70000-".to_owned()),
                Some("bytes=80000-".to_owned())
            ]
        );
        assert!(matches!(
            events.0.as_slice(),
            [
                ResumableEvent::Started {
                    resumed_from: 0,
                    total: Some(200_000),
                    ..
                },
                ResumableEvent::Reconnecting {
                    done: 70_000,
                    attempt: 1
                },
                ResumableEvent::Reconnecting {
                    done: 80_000,
                    attempt: 2
                },
                ResumableEvent::Finished {
                    done: 200_000,
                    resumed_from: 0
                },
            ]
        ));
    }

    #[test]
    fn a_later_run_resumes_a_partial_download() {
        let dir = tempdir();
        let dest = dir.path().join("clone.bundle");
        // The first run's connection keeps dropping until it gives up.
        let failing = server(&[50_000, 0, 0, 0, 0, 0]);
        assert!(matches!(
            download_resumable(&failing, "http://h/b", &dest, &mut NoProgress),
            Err(ResumableError::Exhausted { .. })
        ));
        assert!(!dest.exists());

        let srv = server(&[]);
        let mut events = Events::default();
        let outcome = download_resumable(&srv, "http://h/b", &dest, &mut events);
        assert_eq!(outcome.ok().map(|o| o.resumed_from), Some(50_000));
        assert_eq!(std::fs::read(&dest).ok(), Some(srv.data.clone()));
        assert!(matches!(
            events.0.first(),
            Some(ResumableEvent::Started {
                resumed_from: 50_000,
                ..
            })
        ));

        // Complete already: nothing is requested.
        let again = server(&[]);
        let outcome = download_resumable(&again, "http://h/b", &dest, &mut NoProgress);
        assert_eq!(outcome.ok().map(|o| o.resumed_from), Some(200_000));
        assert!(again
            .ranges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }

    #[test]
    fn a_changed_file_or_url_starts_over() {
        let dir = tempdir();
        let dest = dir.path().join("clone.bundle");
        let failing = server(&[50_000, 0, 0, 0, 0, 0]);
        let _ = download_resumable(&failing, "http://h/b", &dest, &mut NoProgress);

        let mut changed = server(&[]);
        changed.etag = "\"v2\"".to_owned();
        let outcome = download_resumable(&changed, "http://h/b", &dest, &mut NoProgress);
        assert_eq!(outcome.ok().map(|o| o.bytes), Some(200_000));
        assert_eq!(std::fs::read(&dest).ok(), Some(changed.data.clone()));

        let other = server(&[]);
        let outcome = download_resumable(&other, "http://h/other", &dest, &mut NoProgress);
        assert_eq!(outcome.ok().map(|o| o.resumed_from), Some(0));
        assert_eq!(
            other
                .ranges
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_slice(),
            [None]
        );
    }

    #[test]
    fn content_ranges_parse() {
        assert_eq!(
            parse_content_range(Some("bytes 10-99/100")),
            Some((10, Some(100)))
        );
        assert_eq!(parse_content_range(Some("bytes 10-99/*")), Some((10, None)));
        assert_eq!(parse_content_range(Some("items 1-2/3")), None);
        assert_eq!(parse_content_range(None), None);
    }

    #[test]
    fn urls_resolve_against_the_endpoint() {
        assert_eq!(
            resolve_url("http://h:1/repo/push", "/repo/push/abc").as_deref(),
            Some("http://h:1/repo/push/abc")
        );
        assert_eq!(
            resolve_url("http://h/repo/push", "https://other/x").as_deref(),
            Some("https://other/x")
        );
    }

    /// A TUS server in memory that drops some PATCHes after keeping part of them.
    struct TusServer {
        stored: Mutex<Vec<u8>>,
        /// Per PATCH: bytes to keep before failing (`None` = accept all).
        patch_cuts: Mutex<VecDeque<Option<usize>>>,
        length: Mutex<u64>,
    }

    impl HttpClient for TusServer {
        fn get(&self, _: &str, _: Option<&str>) -> crate::error::Result<Vec<u8>> {
            unreachable!()
        }
        fn post(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: Option<&str>,
        ) -> crate::error::Result<Vec<u8>> {
            unreachable!()
        }
        fn send(&self, request: &HttpRequest<'_>) -> crate::error::Result<HttpResponse> {
            let header = |name: &str| {
                request
                    .headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.clone())
            };
            let mut stored = self.stored.lock().unwrap_or_else(|e| e.into_inner());
            let reply = |status: u16, headers: Vec<(String, String)>| HttpResponse {
                status,
                headers,
                body: Box::new(std::io::empty()),
            };
            let offset = |n: usize| vec![("upload-offset".to_owned(), n.to_string())];
            Ok(match request.method {
                HttpMethod::Post => {
                    *self.length.lock().unwrap_or_else(|e| e.into_inner()) =
                        header("Upload-Length")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                    reply(201, vec![("location".to_owned(), "/push/u1".to_owned())])
                }
                HttpMethod::Head => reply(200, offset(stored.len())),
                HttpMethod::Patch => {
                    let at: usize = header("Upload-Offset")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(usize::MAX);
                    if at != stored.len() {
                        return Ok(reply(409, vec![]));
                    }
                    let cut = self
                        .patch_cuts
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .pop_front()
                        .flatten();
                    match cut {
                        Some(keep) => {
                            stored.extend_from_slice(&request.body[..keep]);
                            return Err(crate::error::Error::Message("reset".to_owned()));
                        }
                        None => {
                            stored.extend_from_slice(request.body);
                            reply(204, offset(stored.len()))
                        }
                    }
                }
                HttpMethod::Get | HttpMethod::Put | HttpMethod::Delete => reply(405, vec![]),
            })
        }
    }

    #[test]
    fn tus_uploads_resume_from_the_server_offset() {
        let dir = tempdir();
        let file = dir.path().join("push.bundle");
        let data: Vec<u8> = (0..(PATCH_CHUNK * 2 + 1000))
            .map(|i| (i % 253) as u8)
            .collect();
        assert!(std::fs::write(&file, &data).is_ok());
        let srv = TusServer {
            stored: Mutex::new(Vec::new()),
            patch_cuts: Mutex::new([None, Some(1000)].into_iter().collect()),
            length: Mutex::new(0),
        };
        let upload = match TusUpload::create(&srv, "http://h/push", data.len() as u64) {
            Ok(u) => u,
            Err(e) => panic!("create: {e}"),
        };
        assert_eq!(upload.url, "http://h/push/u1");
        let mut events = Events::default();
        assert!(upload.send_file(&srv, &file, &mut events).is_ok());
        assert_eq!(*srv.stored.lock().unwrap_or_else(|e| e.into_inner()), data);
        assert!(matches!(
            events.0.as_slice(),
            [
                ResumableEvent::Started {
                    resumed_from: 0,
                    ..
                },
                ResumableEvent::Reconnecting { attempt: 1, .. },
                ResumableEvent::Finished { .. },
            ]
        ));
        assert_eq!(upload.offset(&srv).ok().flatten(), Some(data.len() as u64));
    }
}
