//! HTTP/1.1 connection pinned to one socket for multi-leg NTLM or Negotiate auth.
//!
//! After authentication succeeds, later same-origin requests reuse the socket and
//! must not resend `Authorization` (the connection is already authenticated).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;

use url::Url;

use crate::error::{Error, RemoteUnreadableReason, Result};
use crate::remote::RemoteError;
use crate::transport::http::ureq_client::HttpTransportTimeouts;

trait ReadWriteStream: Read + Write + Send {}

impl<T: Read + Write + Send> ReadWriteStream for T {}

type IoStream = Box<dyn ReadWriteStream>;

/// Which `Authorization`-style header to send on the next request.
pub(crate) enum AuthSend<'a> {
    /// Do not send an auth header (including on an authenticated connection).
    Skip,
    Authorization(&'a str),
    ProxyAuthorization(&'a str),
}

/// One pinned keep-alive connection to an HTTP origin (optionally via an HTTP proxy).
pub(crate) struct PinnedConnection {
    stream: IoStream,
    host: String,
    authenticated: bool,
    /// Plain `http://` remotes through a proxy use absolute-form request-URIs.
    absolute_origin: Option<Url>,
}

/// Pool of authenticated pinned connections keyed by origin (`scheme://host:port`).
#[derive(Default)]
pub(crate) struct PinnedPool {
    inner: Mutex<HashMap<String, PinnedConnection>>,
}

impl PinnedPool {
    pub(crate) fn take(&self, origin: &str) -> Option<PinnedConnection> {
        let mut guard = self.inner.lock().ok()?;
        guard.remove(origin)
    }

    pub(crate) fn store(&self, origin: String, conn: PinnedConnection) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(origin, conn);
        }
    }
}

/// Response metadata from a pinned request (body fully buffered).
#[derive(Clone)]
pub(crate) struct PinnedResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub www_authenticate: Vec<String>,
    pub proxy_authenticate: Vec<String>,
}

pub(crate) struct ConnectOptions<'a> {
    pub target: &'a Url,
    pub proxy: Option<&'a Url>,
    pub timeouts: &'a HttpTransportTimeouts,
    pub tunnel_auth: Option<super::auth_exchange::TunnelAuth<'a>>,
}

impl PinnedConnection {
    /// Open a connection to `opts.target`, tunneling through `opts.proxy` when set for HTTPS.
    pub(crate) fn connect(opts: &ConnectOptions<'_>) -> Result<Self> {
        let host = opts
            .target
            .host_str()
            .ok_or_else(|| transport_err("target", "missing host"))?;
        let port = opts.target.port_or_known_default().unwrap_or(443);
        let (stream, absolute_origin) = if let Some(proxy) = opts.proxy {
            if opts.target.scheme() == "https" {
                (
                    connect_via_proxy_https(
                        proxy,
                        host,
                        port,
                        opts.timeouts,
                        opts.tunnel_auth.as_ref(),
                    )?,
                    None,
                )
            } else {
                (
                    connect_to_plain_http_proxy(proxy, opts.timeouts)?,
                    Some(opts.target.clone()),
                )
            }
        } else if opts.target.scheme() == "https" {
            (plain_tls_connect(host, opts.timeouts)?, None)
        } else {
            let plain = TcpStream::connect(format!(
                "{host}:{}",
                opts.target.port_or_known_default().unwrap_or(80)
            ))
            .map_err(|e| transport_err(host, e.to_string()))?;
            configure_timeouts(&plain, opts.timeouts)?;
            (Box::new(plain) as IoStream, None)
        };
        Ok(Self {
            stream,
            host: host.to_owned(),
            authenticated: false,
            absolute_origin,
        })
    }

    pub(crate) fn mark_authenticated(&mut self) {
        self.authenticated = true;
    }

    #[must_use]
    pub(crate) fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Send one HTTP/1.1 request on the pinned socket.
    pub(crate) fn request(
        &mut self,
        method: &str,
        path_query: &str,
        extra_headers: &[(String, String)],
        body: Option<&[u8]>,
        auth: AuthSend<'_>,
    ) -> Result<PinnedResponse> {
        let request_target = if let Some(origin) = &self.absolute_origin {
            absolute_request_uri(origin, path_query)
        } else {
            path_query.to_string()
        };
        let mut req = format!(
            "{method} {request_target} HTTP/1.1\r\nHost: {}\r\n",
            self.host
        );
        req.push_str("Connection: keep-alive\r\n");
        if !self.authenticated {
            match auth {
                AuthSend::Authorization(value) => {
                    req.push_str("Authorization: ");
                    req.push_str(value);
                    req.push_str("\r\n");
                }
                AuthSend::ProxyAuthorization(value) => {
                    req.push_str("Proxy-Authorization: ");
                    req.push_str(value);
                    req.push_str("\r\n");
                }
                AuthSend::Skip => {}
            }
        }
        if let Some(b) = body {
            let _ = write!(req, "Content-Length: {}\r\n", b.len());
        }
        for (k, v) in extra_headers {
            let _ = write!(req, "{k}: {v}\r\n");
        }
        req.push_str("\r\n");
        self.stream
            .write_all(req.as_bytes())
            .map_err(|e| transport_err(&self.host, e.to_string()))?;
        if let Some(b) = body {
            self.stream
                .write_all(b)
                .map_err(|e| transport_err(&self.host, e.to_string()))?;
        }
        self.stream
            .flush()
            .map_err(|e| transport_err(&self.host, e.to_string()))?;
        read_http_response(&mut self.stream)
    }
}

pub(crate) fn send_proxy_connect(
    tcp: &mut TcpStream,
    target_host: &str,
    target_port: u16,
    proxy_authorization: Option<&str>,
    _timeouts: &HttpTransportTimeouts,
) -> Result<PinnedResponse> {
    let mut connect_line = format!(
        "CONNECT {target_host}:{target_port} HTTP/1.1\r\nHost: {target_host}:{target_port}\r\n"
    );
    connect_line.push_str("Connection: keep-alive\r\n");
    if let Some(value) = proxy_authorization {
        connect_line.push_str("Proxy-Authorization: ");
        connect_line.push_str(value);
        connect_line.push_str("\r\n");
    }
    connect_line.push_str("\r\n");
    tcp.write_all(connect_line.as_bytes())
        .map_err(|e| transport_err(target_host, e.to_string()))?;
    tcp.flush()
        .map_err(|e| transport_err(target_host, e.to_string()))?;
    read_http_response(tcp)
}

fn absolute_request_uri(origin: &Url, path_query: &str) -> String {
    let authority = origin.host_str().map(|host| {
        if let Some(port) = origin.port() {
            format!("{host}:{port}")
        } else {
            host.to_string()
        }
    });
    format!(
        "{}://{}{}",
        origin.scheme(),
        authority.unwrap_or_default(),
        path_query
    )
}

fn connect_to_plain_http_proxy(proxy: &Url, timeouts: &HttpTransportTimeouts) -> Result<IoStream> {
    let proxy_host = proxy
        .host_str()
        .ok_or_else(|| transport_err("proxy", "missing host"))?;
    let proxy_port = proxy.port_or_known_default().unwrap_or(8080);
    let tcp = TcpStream::connect(format!("{proxy_host}:{proxy_port}"))
        .map_err(|e| transport_err(proxy_host, e.to_string()))?;
    configure_timeouts(&tcp, timeouts)?;
    Ok(Box::new(tcp) as IoStream)
}

fn connect_via_proxy_https(
    proxy: &Url,
    target_host: &str,
    target_port: u16,
    timeouts: &HttpTransportTimeouts,
    tunnel_auth: Option<&super::auth_exchange::TunnelAuth<'_>>,
) -> Result<IoStream> {
    let proxy_host = proxy
        .host_str()
        .ok_or_else(|| transport_err("proxy", "missing host"))?;
    let proxy_port = proxy.port_or_known_default().unwrap_or(8080);
    let mut tcp = TcpStream::connect(format!("{proxy_host}:{proxy_port}"))
        .map_err(|e| transport_err(proxy_host, e.to_string()))?;
    configure_timeouts(&tcp, timeouts)?;
    let head = send_proxy_connect(&mut tcp, target_host, target_port, None, timeouts)?;
    if head.status == 200 {
        // ready for TLS below
    } else if head.status == 407 {
        let auth = tunnel_auth.ok_or_else(|| {
            transport_err(
                proxy_host,
                "proxy requires authentication but no credential handler was configured",
            )
        })?;
        super::auth_exchange::establish_proxy_connect_tunnel(
            &mut tcp,
            proxy_host,
            target_host,
            target_port,
            timeouts,
            auth,
            &head,
        )?;
    } else {
        return Err(transport_err(
            proxy_host,
            format!("CONNECT failed with HTTP {}", head.status),
        ));
    }
    #[cfg(not(feature = "http-ureq-native-tls"))]
    {
        use rustls::pki_types::ServerName;
        use std::sync::Arc;
        let config = rustls_client_config()?;
        let server = ServerName::try_from(target_host.to_owned())
            .map_err(|_| transport_err(target_host, "invalid TLS server name"))?;
        let conn = rustls::ClientConnection::new(Arc::new(config), server)
            .map_err(|e| transport_err(target_host, e.to_string()))?;
        Ok(Box::new(rustls::StreamOwned::new(conn, tcp)) as IoStream)
    }
    #[cfg(feature = "http-ureq-native-tls")]
    {
        let connector = native_tls::TlsConnector::new()
            .map_err(|e| transport_err(target_host, e.to_string()))?;
        Ok(Box::new(
            connector
                .connect(target_host, tcp)
                .map_err(|e| transport_err(target_host, e.to_string()))?,
        ) as IoStream)
    }
}

#[cfg(not(feature = "http-ureq-native-tls"))]
fn plain_tls_connect(host: &str, timeouts: &HttpTransportTimeouts) -> Result<IoStream> {
    let port = 443;
    let tcp = TcpStream::connect(format!("{host}:{port}"))
        .map_err(|e| transport_err(host, e.to_string()))?;
    configure_timeouts(&tcp, timeouts)?;
    use rustls::pki_types::ServerName;
    use std::sync::Arc;
    let config = rustls_client_config()?;
    let server = ServerName::try_from(host.to_owned())
        .map_err(|_| transport_err(host, "invalid TLS server name"))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), server)
        .map_err(|e| transport_err(host, e.to_string()))?;
    Ok(Box::new(rustls::StreamOwned::new(conn, tcp)) as IoStream)
}

#[cfg(feature = "http-ureq-native-tls")]
fn plain_tls_connect(host: &str, timeouts: &HttpTransportTimeouts) -> Result<IoStream> {
    let tcp = TcpStream::connect(format!("{host}:443"))
        .map_err(|e| transport_err(host, e.to_string()))?;
    configure_timeouts(&tcp, timeouts)?;
    let connector =
        native_tls::TlsConnector::new().map_err(|e| transport_err(host, e.to_string()))?;
    Ok(Box::new(
        connector
            .connect(host, tcp)
            .map_err(|e| transport_err(host, e.to_string()))?,
    ) as IoStream)
}

fn configure_timeouts(stream: &TcpStream, timeouts: &HttpTransportTimeouts) -> Result<()> {
    let _ = stream.set_read_timeout(timeouts.per_read());
    let _ = stream.set_write_timeout(Some(timeouts.connect));
    Ok(())
}

fn read_http_response(stream: &mut dyn Read) -> Result<PinnedResponse> {
    let mut buf = [0u8; 8192];
    let mut collected = Vec::new();
    loop {
        let n = stream
            .read(&mut buf)
            .map_err(|e| transport_err("pinned", e.to_string()))?;
        if n == 0 {
            break;
        }
        collected.extend_from_slice(&buf[..n]);
        if collected.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if collected.len() > 64 * 1024 {
            return Err(transport_err("pinned", "response headers too large"));
        }
    }
    let header_end = collected
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| transport_err("pinned", "malformed response"))?
        + 4;
    let header_text = String::from_utf8_lossy(&collected[..header_end]);
    let status = parse_status(&header_text)?;
    let headers = parse_headers(&header_text);
    let www_authenticate = header_values(&headers, "www-authenticate");
    let proxy_authenticate = header_values(&headers, "proxy-authenticate");
    let content_length = header_values(&headers, "content-length")
        .first()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = collected[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut buf)
            .map_err(|e| transport_err("pinned", e.to_string()))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    if content_length > 0 {
        body.truncate(content_length);
    }
    Ok(PinnedResponse {
        status,
        body,
        www_authenticate,
        proxy_authenticate,
    })
}

fn parse_status(headers: &str) -> Result<u16> {
    let line = headers
        .lines()
        .next()
        .ok_or_else(|| transport_err("pinned", "no status"))?;
    let mut parts = line.split_whitespace();
    let _http = parts.next();
    parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| transport_err("pinned", "bad status line"))
}

fn parse_headers(header_text: &str) -> Vec<(String, String)> {
    header_text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (k, v) = line.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn header_values(headers: &[(String, String)], name: &str) -> Vec<String> {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
        .collect()
}

fn transport_err(url: &str, detail: impl Into<String>) -> Error {
    Error::Remote(RemoteError::Unreadable {
        url: Some(url.to_owned()),
        reason: RemoteUnreadableReason::Transport {
            detail: detail.into(),
        },
    })
}

#[cfg(not(feature = "http-ureq-native-tls"))]
fn rustls_client_config() -> Result<rustls::ClientConfig> {
    use rustls::RootCertStore;
    use std::sync::Arc;
    let mut roots = RootCertStore::empty();
    let certs = rustls_native_certs::load_native_certs();
    for cert in certs.certs {
        let _ = roots.add(cert);
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| transport_err("tls", e.to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

#[cfg(all(test, feature = "http-ureq"))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn absolute_request_uri_includes_scheme_host_and_path() {
        let origin = Url::parse("http://git.example:8080/repo.git").unwrap();
        assert_eq!(
            absolute_request_uri(&origin, "/info/refs?service=git-upload-pack"),
            "http://git.example:8080/info/refs?service=git-upload-pack"
        );
    }

    fn read_first_line(stream: &mut TcpStream) -> String {
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        String::from_utf8_lossy(&buf[..n])
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn https_via_proxy_sends_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let line = read_first_line(&mut s);
                let _ = tx.send(line);
            }
        });
        let proxy = Url::parse(&format!("http://127.0.0.1:{proxy_port}")).unwrap();
        let target = Url::parse("https://upstream.test/repo.git").unwrap();
        let timeouts = HttpTransportTimeouts::default();
        let _ = connect_via_proxy_https(&proxy, "upstream.test", 443, &timeouts, None);
        let line = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
        assert!(
            line.starts_with("CONNECT upstream.test:443 "),
            "expected CONNECT, got {line:?}"
        );
    }

    #[test]
    fn plain_http_via_proxy_uses_absolute_get_not_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let line = read_first_line(&mut s);
                let _ = tx.send(line);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        let proxy = Url::parse(&format!("http://127.0.0.1:{proxy_port}")).unwrap();
        let target = Url::parse("http://upstream.test:8080/repo.git").unwrap();
        let timeouts = HttpTransportTimeouts::default();
        let mut conn = PinnedConnection::connect(&ConnectOptions {
            target: &target,
            proxy: Some(&proxy),
            timeouts: &timeouts,
            tunnel_auth: None,
        })
        .expect("connect");
        let _ = conn.request("GET", "/info/refs", &[], None, AuthSend::Skip);
        let line = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
        assert!(
            !line.starts_with("CONNECT "),
            "plain HTTP must not send CONNECT, got {line:?}"
        );
        assert!(
            line.starts_with("GET http://upstream.test:8080/"),
            "expected absolute-form GET, got {line:?}"
        );
    }

    #[test]
    fn plain_http_proxy_ntlm_407_receives_proxy_authorization() {
        use super::super::ntlm::type1_message;
        use base64::Engine;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let _ = tx.send(if head.contains("Proxy-Authorization:") {
                "unexpected"
            } else {
                "challenge"
            });
            let challenge =
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: NTLM\r\nContent-Length: 0\r\n\r\n";
            let _ = s.write_all(challenge);
            let _ = s.flush();

            let n = s.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let _ = tx.send(if head.contains("Proxy-Authorization: NTLM ") {
                "authed"
            } else {
                "missing-proxy-auth"
            });
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        });
        let proxy = Url::parse(&format!("http://127.0.0.1:{proxy_port}")).unwrap();
        let target = Url::parse("http://upstream.test/repo.git").unwrap();
        let timeouts = HttpTransportTimeouts::default();
        let mut conn = PinnedConnection::connect(&ConnectOptions {
            target: &target,
            proxy: Some(&proxy),
            timeouts: &timeouts,
            tunnel_auth: None,
        })
        .expect("connect");
        let type1_hdr = format!(
            "NTLM {}",
            base64::engine::general_purpose::STANDARD.encode(type1_message())
        );
        let first = conn
            .request("GET", "/info/refs", &[], None, AuthSend::Skip)
            .expect("first request");
        assert_eq!(first.status, 407);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "challenge"
        );
        let _ = conn.request(
            "GET",
            "/info/refs",
            &[],
            None,
            AuthSend::ProxyAuthorization(&type1_hdr),
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "authed",
            "second request must carry Proxy-Authorization after 407"
        );
    }
}
