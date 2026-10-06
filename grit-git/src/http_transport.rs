//! HTTP transport timeouts and response-body spooling for large upload-pack downloads.

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use grit_lib::config::{parse_i64, ConfigSet};
use tempfile::NamedTempFile;

/// Git-shaped HTTP timeouts and optional low-speed abort thresholds (`http.*` config).
#[derive(Clone, Debug)]
pub(crate) struct HttpTransportTimeouts {
    pub connect: Duration,
    /// Whole-request ceiling (`http.timeout`, Git default 600s; 0 disables).
    pub global: Option<Duration>,
    /// When set, curl-style low speed limit (bytes per second) and time window (seconds).
    pub low_speed: Option<LowSpeedSettings>,
}

/// Active `http.lowSpeedLimit` / `http.lowSpeedTime` pair (both must be positive).
#[derive(Clone, Copy, Debug)]
pub(crate) struct LowSpeedSettings {
    pub bytes_per_second: u64,
    pub window: Duration,
}

impl HttpTransportTimeouts {
    /// Read timeout-related settings from `config`, matching Git/curl defaults.
    pub fn from_config(config: &ConfigSet) -> Self {
        let global = config
            .get("http.timeout")
            .as_deref()
            .and_then(|v| parse_i64(v).ok())
            .map(|secs| {
                if secs <= 0 {
                    None
                } else {
                    Some(Duration::from_secs(secs as u64))
                }
            })
            .unwrap_or(Some(Duration::from_secs(600)));

        Self {
            connect: Duration::from_secs(30),
            global,
            low_speed: parse_low_speed(config),
        }
    }

    /// Socket read/write timeout for manual HTTP-over-TCP proxy and SOCKS paths.
    pub fn socket_rw_timeout(&self) -> Option<Duration> {
        self.global
    }
}

fn parse_low_speed(config: &ConfigSet) -> Option<LowSpeedSettings> {
    let limit = env::var("GIT_HTTP_LOW_SPEED_LIMIT")
        .ok()
        .and_then(|v| parse_i64(v.trim()).ok())
        .or_else(|| {
            config
                .get("http.lowSpeedLimit")
                .and_then(|v| parse_i64(v.trim()).ok())
        });
    let time_secs = env::var("GIT_HTTP_LOW_SPEED_TIME")
        .ok()
        .and_then(|v| parse_i64(v.trim()).ok())
        .or_else(|| {
            config
                .get("http.lowSpeedTime")
                .and_then(|v| parse_i64(v.trim()).ok())
        });

    match (limit, time_secs) {
        (Some(lim), Some(secs)) if lim > 0 && secs > 0 => Some(LowSpeedSettings {
            bytes_per_second: lim as u64,
            window: Duration::from_secs(secs as u64),
        }),
        (Some(0), _) | (_, Some(0)) => None,
        _ => None,
    }
}

/// Response body stored inline or spooled to a temp file (large upload-pack results).
pub(crate) enum HttpResponseBody {
    Inline(Vec<u8>),
    Spooled { file: NamedTempFile, len: u64 },
}

impl HttpResponseBody {
    #[must_use]
    pub fn len(&self) -> u64 {
        match self {
            Self::Inline(v) => v.len() as u64,
            Self::Spooled { len, .. } => *len,
        }
    }

    pub fn into_vec(self) -> Result<Vec<u8>> {
        match self {
            Self::Inline(v) => Ok(v),
            Self::Spooled { mut file, .. } => {
                file.seek(io::SeekFrom::Start(0))
                    .context("rewind spooled upload-pack body")?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)
                    .context("read spooled upload-pack body")?;
                Ok(buf)
            }
        }
    }

    pub fn reader(&mut self) -> Result<Box<dyn Read + '_>> {
        match self {
            Self::Inline(v) => Ok(Box::new(io::Cursor::new(v.as_slice()))),
            Self::Spooled { file, .. } => {
                file.seek(io::SeekFrom::Start(0))
                    .context("rewind spooled upload-pack body")?;
                Ok(Box::new(file))
            }
        }
    }

    pub fn trace_prefix(&self, max: usize) -> Option<&[u8]> {
        match self {
            Self::Inline(v) => Some(if v.len() <= max {
                v.as_slice()
            } else {
                &v[..max]
            }),
            Self::Spooled { .. } => None,
        }
    }
}

/// Copy `reader` to an inline buffer or a temp file when `spool` is true.
pub(crate) fn read_http_body(
    reader: impl Read,
    what: &str,
    spool: bool,
    timeouts: &HttpTransportTimeouts,
) -> Result<HttpResponseBody> {
    let mut reader = LowSpeedReader::new(reader, timeouts);
    if !spool {
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .with_context(|| format!("read {what} body"))?;
        return Ok(HttpResponseBody::Inline(body));
    }

    let mut file = NamedTempFile::new().context("create upload-pack spool file")?;
    let len = io::copy(&mut reader, &mut file).with_context(|| format!("spool {what} body"))?;
    Ok(HttpResponseBody::Spooled { file, len })
}

/// Wraps a reader to abort when average throughput stays below the configured bytes/sec
/// for the configured window, mirroring curl `LOW_SPEED_LIMIT` / `LOW_SPEED_TIME`.
struct LowSpeedReader<R> {
    inner: R,
    settings: Option<LowSpeedSettings>,
    window_start: Instant,
    window_bytes: u64,
    global_deadline: Option<Instant>,
}

impl<R: Read> LowSpeedReader<R> {
    fn new(inner: R, timeouts: &HttpTransportTimeouts) -> Self {
        let global_deadline = timeouts.global.and_then(|g| Instant::now().checked_add(g));
        Self {
            inner,
            settings: timeouts.low_speed,
            window_start: Instant::now(),
            window_bytes: 0,
            global_deadline,
        }
    }

    fn check_global(&self) -> io::Result<()> {
        if let Some(deadline) = self.global_deadline {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "HTTP transfer exceeded http.timeout",
                ));
            }
        }
        Ok(())
    }

    fn finish_window(&mut self) -> io::Result<()> {
        let Some(settings) = self.settings else {
            self.window_start = Instant::now();
            self.window_bytes = 0;
            return Ok(());
        };
        let elapsed = self.window_start.elapsed();
        if elapsed < settings.window {
            return Ok(());
        }
        let secs = elapsed.as_secs_f64();
        if secs <= 0.0 {
            self.window_start = Instant::now();
            self.window_bytes = 0;
            return Ok(());
        }
        let rate = self.window_bytes as f64 / secs;
        if rate < settings.bytes_per_second as f64 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP transfer speed below http.lowSpeedLimit for http.lowSpeedTime",
            ));
        }
        self.window_start = Instant::now();
        self.window_bytes = 0;
        Ok(())
    }
}

impl<R: Read> Read for LowSpeedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check_global()?;
        self.finish_window()?;
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.window_bytes += n as u64;
            self.finish_window()?;
        }
        Ok(n)
    }
}

/// Whether to print side-band channel-2 progress lines.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SidebandProgress {
    pub show: bool,
}

impl SidebandProgress {
    #[must_use]
    pub fn stderr_enabled(self) -> bool {
        self.show || io::stderr().is_terminal()
    }
}

/// Spool a side-band pack stream into `dest` (file or memory buffer).
pub(crate) fn read_sideband_pack_to_writer(
    r: &mut impl Read,
    dest: &mut impl Write,
    progress: SidebandProgress,
) -> Result<()> {
    let mut seen_pack = false;
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let mut len_buf = [0u8; 4];
        match r.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let len_str = std::str::from_utf8(&len_buf)?;
        let len = usize::from_str_radix(len_str, 16)?;
        match len {
            0 => {
                if seen_pack {
                    break;
                }
                continue;
            }
            1 | 2 => continue,
            n if n <= 4 => anyhow::bail!("invalid pkt-line length in side-band stream: {n}"),
            _ => {}
        }
        let mut payload = vec![0u8; len - 4];
        r.read_exact(&mut payload)?;
        if payload.is_empty() {
            continue;
        }
        match payload[0] {
            1 => {
                let data = &payload[1..];
                if !seen_pack {
                    pending.extend_from_slice(data);
                    if let Some(pos) = pending.windows(4).position(|w| w == b"PACK") {
                        seen_pack = true;
                        dest.write_all(&pending[pos..])?;
                        pending.clear();
                    } else if pending.len() > 3 {
                        let keep_from = pending.len() - 3;
                        pending.drain(..keep_from);
                    }
                } else {
                    dest.write_all(data)?;
                }
            }
            2 => {
                let msg = String::from_utf8_lossy(&payload[1..]);
                let trimmed = msg.trim_end_matches('\n');
                if !trimmed.is_empty() && progress.stderr_enabled() {
                    eprint!("remote: {trimmed}\r");
                    let _ = io::stderr().flush();
                }
            }
            3 => {
                let msg = String::from_utf8_lossy(&payload[1..]).trim().to_string();
                if msg.is_empty() {
                    anyhow::bail!("remote side-band fatal: empty error message");
                }
                anyhow::bail!("remote error: {msg}");
            }
            _ => {
                if !seen_pack {
                    pending.extend_from_slice(&payload);
                    if let Some(pos) = pending.windows(4).position(|w| w == b"PACK") {
                        seen_pack = true;
                        dest.write_all(&pending[pos..])?;
                        pending.clear();
                    } else if pending.len() > 3 {
                        let keep_from = pending.len() - 3;
                        pending.drain(..keep_from);
                    }
                } else {
                    dest.write_all(&payload)?;
                }
            }
        }
    }
    if !seen_pack {
        anyhow::bail!("missing packfile in side-band stream");
    }
    Ok(())
}

/// Decode side-band pack data from `reader` into a temp `.pack` under `git_dir/objects/pack/`.
pub(crate) fn write_sideband_pack_to_temp(
    git_dir: &Path,
    reader: &mut impl Read,
    progress: SidebandProgress,
) -> Result<PathBuf> {
    let pack_dir = git_dir.join("objects/pack");
    fs::create_dir_all(&pack_dir).with_context(|| format!("create {}", pack_dir.display()))?;
    let mut tmp = NamedTempFile::new_in(&pack_dir).context("create temp pack file")?;
    read_sideband_pack_to_writer(reader, &mut tmp, progress)?;
    tmp.flush().context("flush temp pack file")?;
    let path = tmp.path().to_path_buf();
    tmp.persist(&path)
        .map_err(|e| anyhow::anyhow!("persist temp pack file: {}", e.error))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grit_lib::config::{ConfigFile, ConfigScope};
    use std::io::{Cursor, Write};

    fn config_from_snippet(text: &str) -> ConfigSet {
        let file = ConfigFile::parse(
            std::path::Path::new(".git/config"),
            text,
            ConfigScope::Local,
        )
        .expect("parse config snippet");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn http_timeout_defaults_match_git() {
        let t = HttpTransportTimeouts::from_config(&ConfigSet::new());
        assert_eq!(t.connect, Duration::from_secs(30));
        assert_eq!(t.global, Some(Duration::from_secs(600)));
        assert!(t.low_speed.is_none());
    }

    #[test]
    fn http_timeout_zero_disables_global() {
        let t = HttpTransportTimeouts::from_config(&config_from_snippet("[http]\n\ttimeout = 0\n"));
        assert_eq!(t.global, None);
    }

    #[test]
    fn low_speed_only_when_both_configured() {
        let t = HttpTransportTimeouts::from_config(&config_from_snippet(
            "[http]\n\tlowSpeedLimit = 1000\n\tlowSpeedTime = 30\n",
        ));
        let ls = t.low_speed.expect("low speed enabled");
        assert_eq!(ls.bytes_per_second, 1000);
        assert_eq!(ls.window, Duration::from_secs(30));
    }

    #[test]
    fn spooled_body_round_trips_through_into_vec() {
        let data = b"001e# service=git-upload-pack\n0000PACK";
        let timeouts = HttpTransportTimeouts::from_config(&ConfigSet::new());
        let body = read_http_body(Cursor::new(&data[..]), "test", true, &timeouts).unwrap();
        assert_eq!(body.into_vec().unwrap(), data);
    }

    struct BytePerSecond(u64);

    impl Read for BytePerSecond {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(100));
            let n = 1.min(buf.len());
            buf[0] = b'x';
            self.0 = self.0.saturating_sub(1);
            if self.0 == 0 {
                return Ok(0);
            }
            Ok(n)
        }
    }

    #[test]
    fn sideband_fatal_error_on_band_three() {
        let mut stream = Vec::new();
        let payload = b"fatal from server\n";
        let len = 4 + 1 + payload.len();
        write!(stream, "{len:04x}").unwrap();
        stream.push(3);
        stream.extend_from_slice(payload);
        stream.extend_from_slice(b"0000");
        let mut sink = Vec::new();
        let err = read_sideband_pack_to_writer(
            &mut Cursor::new(stream),
            &mut sink,
            SidebandProgress::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("fatal from server"));
    }

    #[test]
    fn sideband_missing_pack_is_error() {
        let mut stream = Vec::new();
        let progress = b"counting\n";
        let len = 4 + 1 + progress.len();
        write!(stream, "{len:04x}").unwrap();
        stream.push(2);
        stream.extend_from_slice(progress);
        stream.extend_from_slice(b"0000");
        let mut sink = Vec::new();
        let err = read_sideband_pack_to_writer(
            &mut Cursor::new(stream),
            &mut sink,
            SidebandProgress::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing packfile"));
    }

    #[test]
    fn low_speed_aborts_slow_transfer() {
        let timeouts = HttpTransportTimeouts {
            connect: Duration::from_secs(30),
            global: None,
            low_speed: Some(LowSpeedSettings {
                bytes_per_second: 50,
                window: Duration::from_millis(200),
            }),
        };
        let mut reader = LowSpeedReader::new(BytePerSecond(100), &timeouts);
        let mut sink = Vec::new();
        let err = reader.read_to_end(&mut sink).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
