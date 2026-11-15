//! TLS and WebSocket: the phone's one connection.

use std::io;
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::json;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

use crate::remote::auth;
use crate::remote::hub::Hub;
use crate::remote::wire::{Hello, Req};
use crate::util;

type Ws = WebSocket<StreamOwned<ServerConnection, TcpStream>>;

/// The first message must arrive this soon.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the connection loop looks for messages to send.
const TICK: Duration = Duration::from_millis(40);
/// A phone that sends nothing for this long is gone (it pings every 30 s).
const IDLE_LIMIT: Duration = Duration::from_secs(90);
/// Connections at once.
const MAX_CONNS: usize = 4;
const MESSAGE_MAX: usize = 1 << 20;

pub fn tls_config(cert_der: &[u8], key_der: &[u8]) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert_der.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der.to_vec())),
        )
        .context("the gateway certificate")?;
    Ok(Arc::new(config))
}

/// Listen on the configured address, waiting for it while the VPN is down.
pub fn serve(hub: Arc<Hub>, tls: Arc<ServerConfig>) {
    let mut reported = false;
    loop {
        match TcpListener::bind(&hub.opts.listen) {
            Ok(listener) => {
                eprintln!("cepheus remote: listening on {}", hub.opts.listen);
                accept_all(&hub, &tls, listener);
            }
            Err(err) => {
                if !reported {
                    eprintln!("cepheus remote: cannot listen on {} yet: {err}", hub.opts.listen);
                    reported = true;
                }
                thread::sleep(Duration::from_secs(10));
            }
        }
    }
}

pub fn accept_all(hub: &Arc<Hub>, tls: &Arc<ServerConfig>, listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Ok(peer) = stream.peer_addr() else { continue };
        let ip = auth::plain_ip(peer.ip());
        // Every VPN peer reaches this address: anything but the phone is
        // dropped before TLS, and so is an address locked out.
        if !auth::allowed(&hub.opts.allow, ip) || !hub.limiter_open(ip) || hub.connections() >= MAX_CONNS {
            continue;
        }
        let (hub, tls) = (Arc::clone(hub), Arc::clone(tls));
        thread::spawn(move || {
            if let Err(err) = connection(&hub, &tls, stream, ip) {
                if !dropped(&err) {
                    eprintln!("cepheus remote: connection from {ip}: {err:#}");
                }
            }
        });
    }
}

/// A phone that leaves the network or is swiped away ends its connection
/// without a TLS close. The emulator check logged "IO error: peer closed
/// connection without sending TLS close_notify" for each such ordinary
/// leave, which reads like a fault in `remote.log`.
pub(crate) fn dropped(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let io = cause.downcast_ref::<io::Error>().or(match cause.downcast_ref::<tungstenite::Error>() {
            Some(tungstenite::Error::Io(e)) => Some(e),
            _ => None,
        });
        io.is_some_and(|e| {
            matches!(
                e.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
            )
        }) || matches!(
            cause.downcast_ref::<tungstenite::Error>(),
            Some(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed)
        )
    })
}

// The signature is tungstenite's handshake `Callback`, which fixes the error type.
#[allow(clippy::result_large_err)]
fn only_v1(req: &Request, resp: Response) -> Result<Response, ErrorResponse> {
    if req.uri().path() == "/v1" {
        Ok(resp)
    } else {
        let mut err = ErrorResponse::new(Some("not found".into()));
        *err.status_mut() = tungstenite::http::StatusCode::NOT_FOUND;
        Err(err)
    }
}

fn timed_out(err: &tungstenite::Error) -> bool {
    matches!(err, tungstenite::Error::Io(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

fn send(ws: &mut Ws, text: String) -> Result<()> {
    ws.send(Message::text(text)).context("sending")
}

fn connection(hub: &Arc<Hub>, tls: &Arc<ServerConfig>, tcp: TcpStream, ip: IpAddr) -> Result<()> {
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(AUTH_TIMEOUT))?;
    tcp.set_write_timeout(Some(Duration::from_secs(20)))?;
    let conn = ServerConnection::new(Arc::clone(tls))?;
    let stream = StreamOwned::new(conn, tcp);
    let config = WebSocketConfig::default().max_message_size(Some(MESSAGE_MAX)).max_frame_size(Some(MESSAGE_MAX));
    let mut ws = tungstenite::accept_hdr_with_config(stream, only_v1, Some(config)).context("WebSocket handshake")?;

    let first = match ws.read() {
        Ok(Message::Text(t)) => t.to_string(),
        Ok(_) => bail!("the first message was not text"),
        Err(err) => return Err(err).context("waiting for auth"),
    };
    let (device, last_seq, epoch) = match serde_json::from_str::<Hello>(&first) {
        Ok(Hello::Auth { device, key, last_seq, epoch }) => {
            if !hub.sign_in(&device, &key) {
                return deny(hub, &mut ws, ip, "unknown phone or key; pair again");
            }
            (device, last_seq, epoch)
        }
        Ok(Hello::Pair { code, name }) => match auth::redeem_code(&hub.opts.dir, &code, util::now_secs()) {
            Ok(()) => {
                let (record, key) = auth::new_device(&name, util::now_secs())?;
                let id = record.id.clone();
                hub.add_device(record)?;
                send(&mut ws, json!({"t": "paired", "device": id, "key": key}).to_string())?;
                (id, None, None)
            }
            Err(err) => return deny(hub, &mut ws, ip, &format!("{err:#}")),
        },
        Err(_) => return deny(hub, &mut ws, ip, "expected auth or pair"),
    };
    hub.limiter_succeed(ip);

    let (tx, rx) = mpsc::channel::<String>();
    let conn_id = hub.add_conn(&device, tx.clone(), last_seq, epoch.as_deref());
    ws.get_mut().sock.set_read_timeout(Some(TICK))?;
    let result = run(hub, &mut ws, conn_id, &tx, &rx);
    hub.remove_conn(conn_id);
    let _ = ws.close(None);
    let _ = ws.flush();
    result
}

fn deny(hub: &Hub, ws: &mut Ws, ip: IpAddr, reason: &str) -> Result<()> {
    hub.limiter_fail(ip);
    let _ = send(ws, json!({"t": "denied", "reason": reason}).to_string());
    let _ = ws.close(None);
    let _ = ws.flush();
    bail!("denied: {reason}")
}

fn run(hub: &Arc<Hub>, ws: &mut Ws, conn: u64, tx: &mpsc::Sender<String>, rx: &mpsc::Receiver<String>) -> Result<()> {
    let mut heard = Instant::now();
    let mut pinged = Instant::now();
    loop {
        // Its pong counts as word from the phone, even while it sleeps.
        if pinged.elapsed() >= hub.opts.ping_every {
            ws.send(Message::Ping(Default::default())).context("pinging")?;
            pinged = Instant::now();
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                heard = Instant::now();
                match serde_json::from_str::<Req>(text.as_str()) {
                    Ok(req) => hub.handle(conn, req, tx),
                    Err(err) => {
                        let _ = tx
                            .send(json!({"t": "err", "rid": null, "error": format!("bad request: {err}")}).to_string());
                    }
                }
            }
            // A piece of a file the phone attaches.
            Ok(Message::Binary(frame)) => {
                heard = Instant::now();
                match crate::remote::wire::piece_frame(&frame) {
                    Ok((piece, data)) => hub.piece(conn, piece, data, tx),
                    Err(err) => {
                        let _ = tx
                            .send(json!({"t": "err", "rid": null, "error": format!("bad request: {err}")}).to_string());
                    }
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => heard = Instant::now(),
            Err(err) if timed_out(&err) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return Ok(()),
            Err(err) => return Err(err).context("reading"),
        }
        while let Ok(text) = rx.try_recv() {
            send(ws, text)?;
        }
        if heard.elapsed() > IDLE_LIMIT {
            bail!("no word from the phone for {} seconds", IDLE_LIMIT.as_secs());
        }
        if !hub.conn_alive(conn) {
            let _ = send(ws, json!({"t": "denied", "reason": "this phone was removed in the deck"}).to_string());
            return Ok(());
        }
    }
}
