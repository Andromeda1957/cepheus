//! Connections the live activity consumer owns itself: a WebSocket and a
//! long-lived event stream it must be able to stop at any moment, so every
//! read has a short timeout and the reader checks whether it is still wanted.
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// How long one read waits before the reader checks whether it is still wanted.
pub const POLL: Duration = Duration::from_millis(200);
const HEAD_MAX: usize = 64 * 1024;

pub enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(tcp) => tcp,
            Stream::Tls(tls) => tls.get_ref(),
        }
    }

    pub fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.tcp().set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
    }

    pub fn shutdown(&self) {
        let _ = self.tcp().shutdown(std::net::Shutdown::Both);
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(tcp) => tcp.read(buf),
            Stream::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(tcp) => tcp.write(buf),
            Stream::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(tcp) => tcp.flush(),
            Stream::Tls(tls) => tls.flush(),
        }
    }
}

/// Public web PKI: the native app's servers are verified like a browser would.
fn tls_config() -> io::Result<Arc<ClientConfig>> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    if let Some(config) = CONFIG.get() {
        return Ok(Arc::clone(config));
    }
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::clone(CONFIG.get_or_init(|| Arc::new(config))))
}

pub fn is_timeout(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "connection timed out"))
}

/// Connect, and for `tls` finish the handshake, before `deadline`.
pub fn connect(host: &str, port: u16, tls: bool, deadline: Instant) -> io::Result<Stream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no address");
    for address in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, remaining(deadline)?) {
            Ok(tcp) => {
                tcp.set_nodelay(true)?;
                tcp.set_read_timeout(Some(remaining(deadline)?))?;
                tcp.set_write_timeout(Some(remaining(deadline)?))?;
                if !tls {
                    return Ok(Stream::Plain(tcp));
                }
                let name = ServerName::try_from(host.to_string()).map_err(io::Error::other)?;
                let mut connection = ClientConnection::new(tls_config()?, name).map_err(io::Error::other)?;
                let mut tcp = tcp;
                while connection.is_handshaking() {
                    tcp.set_read_timeout(Some(remaining(deadline)?))?;
                    connection.complete_io(&mut tcp)?;
                }
                return Ok(Stream::Tls(Box::new(StreamOwned::new(connection, tcp))));
            }
            Err(err) => last = err,
        }
    }
    Err(last)
}

/// A URL's parts for `connect`.
pub struct Target {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// The path and query, `/` at least.
    pub path: String,
}

impl Target {
    pub fn parse(url: &str) -> Option<Target> {
        let uri: tungstenite::http::Uri = url.parse().ok()?;
        let tls = match uri.scheme_str()? {
            "https" | "wss" => true,
            "http" | "ws" => false,
            _ => return None,
        };
        let host = uri.host()?.trim_start_matches('[').trim_end_matches(']').to_string();
        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
        let path = uri.path_and_query().map_or("/".to_string(), |p| p.as_str().to_string());
        Some(Target { tls, host, port, path })
    }
}

/// A response's status and lowercase headers, and the body bytes read with them.
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub rest: Vec<u8>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

/// Send one request with an empty body and read the response head, giving
/// up when `wanted` turns false or `deadline` passes.
pub fn request(
    stream: &mut Stream,
    method: &str,
    target: &Target,
    headers: &[(&str, String)],
    deadline: Instant,
    wanted: &dyn Fn() -> bool,
) -> io::Result<Head> {
    let mut text = format!("{method} {} HTTP/1.1\r\nHost: {}\r\n", target.path, target.host);
    for (name, value) in headers {
        if value.contains(['\r', '\n']) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid header"));
        }
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("Accept: */*\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(text.as_bytes())?;
    stream.flush()?;
    stream.set_read_timeout(POLL)?;
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    let end = loop {
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if data.len() > HEAD_MAX {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "response head too large"));
        }
        if !wanted() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "no longer wanted"));
        }
        remaining(deadline)?;
        match stream.read(&mut buf) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(err) if is_timeout(&err) => {}
            Err(err) => return Err(err),
        }
    };
    let head = String::from_utf8_lossy(&data[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid status line"))?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Ok(Head { status, headers, rest: data[end + 4..].to_vec() })
}

/// The body of a response read incrementally: chunked, a known length, or
/// until the connection closes.
pub enum Body {
    Chunked { line: Vec<u8>, left: u64, crlf: u8, trailer: bool, done: bool },
    Length(u64),
    Close,
}

impl Body {
    pub fn for_head(head: &Head) -> Body {
        if head.header("transfer-encoding").is_some_and(|value| value.to_ascii_lowercase().contains("chunked")) {
            Body::Chunked { line: Vec::new(), left: 0, crlf: 0, trailer: false, done: false }
        } else if let Some(length) = head.header("content-length").and_then(|v| v.parse().ok()) {
            Body::Length(length)
        } else {
            Body::Close
        }
    }

    /// Whether the whole body has arrived.
    pub fn done(&self) -> bool {
        matches!(self, Body::Chunked { done: true, .. } | Body::Length(0))
    }

    /// Decode raw bytes into body bytes.
    pub fn feed(&mut self, mut raw: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid chunked body");
        match self {
            Body::Close => out.extend_from_slice(raw),
            Body::Length(left) => {
                let take = raw.len().min(*left as usize);
                out.extend_from_slice(&raw[..take]);
                *left -= take as u64;
            }
            Body::Chunked { line, left, crlf, trailer, done } => {
                while !raw.is_empty() && !*done {
                    if *left > 0 {
                        let take = raw.len().min(*left as usize);
                        out.extend_from_slice(&raw[..take]);
                        *left -= take as u64;
                        raw = &raw[take..];
                        if *left == 0 {
                            *crlf = 2;
                        }
                    } else if *crlf > 0 {
                        match raw[0] {
                            b'\r' if *crlf == 2 => *crlf = 1,
                            b'\n' => *crlf = 0,
                            _ => return Err(invalid()),
                        }
                        raw = &raw[1..];
                    } else {
                        let byte = raw[0];
                        raw = &raw[1..];
                        if byte != b'\n' {
                            line.push(byte);
                            if line.len() > 1024 {
                                return Err(invalid());
                            }
                            continue;
                        }
                        let text = String::from_utf8_lossy(line).trim_end_matches('\r').to_string();
                        line.clear();
                        if *trailer {
                            if text.is_empty() {
                                *done = true;
                            }
                            continue;
                        }
                        let size = text.split(';').next().unwrap_or("").trim();
                        let size = u64::from_str_radix(size, 16).map_err(|_| invalid())?;
                        if size == 0 {
                            *trailer = true;
                        } else {
                            *left = size;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
