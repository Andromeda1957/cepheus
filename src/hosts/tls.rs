//! TLS between the hub and a host's peer listener. The hub pins the host's
//! self-signed certificate by its SHA-256; once the handshake is over, a
//! bridge thread carries the plaintext to a local socket, so both ends then
//! speak to a plain `UnixStream` exactly as the deck speaks to its service.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, Connection, DigitallySignedStruct, ServerConfig, SignatureScheme};

use crate::remote::auth;

/// How long connecting and the handshake may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// A write the other end does not take within this ends the connection.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Accepts exactly one certificate: the one whose SHA-256 was recorded when
/// the host was installed. Signatures are still checked, so only the holder
/// of that certificate's key completes the handshake.
#[derive(Debug)]
pub struct Pinned {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let seen = auth::sha256_hex(end_entity.as_ref());
        if auth::ct_eq(seen.as_bytes(), self.fingerprint.to_ascii_lowercase().as_bytes()) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("the host's certificate is not the one recorded at install".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub fn client_config(fingerprint: &str) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pinned { fingerprint: fingerprint.to_string(), provider: Arc::clone(&provider) });
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Keystrokes go out at once, and a host that vanished (Wi-Fi gone, lid
/// shut) is noticed within about half a minute rather than never.
pub fn tune(tcp: &TcpStream) {
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_write_timeout(Some(WRITE_TIMEOUT));
    let fd = tcp.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, value: libc::c_int| {
        // SAFETY: setsockopt reads one c_int from a live local.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&value as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 10);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 5);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 4);
    set(libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, 30_000);
}

/// Connect to the first of `addresses` that answers and complete the
/// handshake, the host's certificate checked against `fingerprint`.
pub fn connect(addresses: &[String], fingerprint: &str) -> Result<(Connection, TcpStream)> {
    let config = client_config(fingerprint)?;
    let mut last = anyhow!("no address");
    for address in addresses {
        let resolved: Vec<SocketAddr> = match address.to_socket_addrs() {
            Ok(found) => found.collect(),
            Err(err) => {
                last = anyhow!("{address}: {err}");
                continue;
            }
        };
        for addr in resolved {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(tcp) => {
                    let name = ServerName::try_from("cepheus").map_err(|e| anyhow!("{e}"))?;
                    let conn = ClientConnection::new(Arc::clone(&config), name).context("TLS")?;
                    let mut conn = Connection::Client(conn);
                    tune(&tcp);
                    let mut tcp = tcp;
                    handshake(&mut conn, &mut tcp).with_context(|| address.to_string())?;
                    return Ok((conn, tcp));
                }
                Err(err) => last = anyhow!("{address}: {err}"),
            }
        }
    }
    Err(last)
}

/// Complete the handshake within `CONNECT_TIMEOUT`.
pub fn handshake(conn: &mut Connection, tcp: &mut TcpStream) -> Result<()> {
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    while conn.is_handshaking() {
        conn.complete_io(tcp).map_err(|e| match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => anyhow!("the TLS handshake timed out"),
            _ => anyhow!("{e}"),
        })?;
    }
    while conn.wants_write() {
        conn.write_tls(tcp)?;
    }
    tcp.set_read_timeout(None)?;
    Ok(())
}

/// Hand the connection's plaintext to a thread that carries it to and from
/// the returned socket until either side closes.
pub fn bridged(conn: Connection, tcp: TcpStream) -> Result<UnixStream> {
    let (ours, theirs) = UnixStream::pair()?;
    thread::Builder::new().name("tls-bridge".into()).spawn(move || {
        let _ = bridge(conn, tcp, theirs);
    })?;
    Ok(ours)
}

/// Which of two descriptors can be read without blocking (or have hung up).
fn poll2(a: RawFd, b: RawFd) -> io::Result<(bool, bool)> {
    let mut fds = [
        libc::pollfd { fd: a, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: b, events: libc::POLLIN, revents: 0 },
    ];
    loop {
        // SAFETY: poll reads and writes the two pollfd entries of `fds`.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if rc >= 0 {
            break;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    let ready = |f: &libc::pollfd| f.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
    Ok((ready(&fds[0]), ready(&fds[1])))
}

fn flush_tls(conn: &mut Connection, tcp: &mut TcpStream) -> io::Result<()> {
    while conn.wants_write() {
        conn.write_tls(tcp)?;
    }
    Ok(())
}

/// Pass on the plaintext the connection holds; whether the other side has
/// closed.
fn deliver(conn: &mut Connection, local: &mut UnixStream) -> io::Result<bool> {
    let state = conn.process_new_packets().map_err(io::Error::other)?;
    let waiting = state.plaintext_bytes_to_read();
    let closed = state.peer_has_closed();
    if waiting > 0 {
        let mut data = vec![0u8; waiting];
        conn.reader().read_exact(&mut data)?;
        local.write_all(&data)?;
    }
    Ok(closed)
}

/// Carry plaintext between the TLS connection on `tcp` and `local`.
pub fn bridge(mut conn: Connection, mut tcp: TcpStream, mut local: UnixStream) -> io::Result<()> {
    let result = (|| -> io::Result<()> {
        let mut buf = vec![0u8; 64 * 1024];
        // What came in with the end of the handshake is already decrypted,
        // and the socket may not become readable again until it is answered.
        if deliver(&mut conn, &mut local)? {
            return Ok(());
        }
        loop {
            flush_tls(&mut conn, &mut tcp)?;
            let (net, ours) = poll2(tcp.as_raw_fd(), local.as_raw_fd())?;
            if net {
                match conn.read_tls(&mut tcp) {
                    Ok(0) => return Ok(()),
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                    Err(e) => return Err(e),
                }
                if deliver(&mut conn, &mut local)? {
                    return Ok(());
                }
            }
            if ours {
                match local.read(&mut buf) {
                    Ok(0) => {
                        conn.send_close_notify();
                        return flush_tls(&mut conn, &mut tcp);
                    }
                    Ok(n) => conn.writer().write_all(&buf[..n])?,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
        }
    })();
    let _ = tcp.shutdown(Shutdown::Both);
    let _ = local.shutdown(Shutdown::Both);
    result
}

/// The peer's side: the handshake for one accepted connection.
pub fn accept(config: &Arc<ServerConfig>, mut tcp: TcpStream) -> Result<(Connection, TcpStream)> {
    tune(&tcp);
    let conn = rustls::ServerConnection::new(Arc::clone(config)).context("TLS")?;
    let mut conn = Connection::Server(conn);
    handshake(&mut conn, &mut tcp)?;
    if conn.is_handshaking() {
        bail!("handshake incomplete");
    }
    Ok((conn, tcp))
}

#[cfg(test)]
#[path = "../tests/hosts_tls.rs"]
mod tests;
