//! The hub service's connections to a host, each a thread that reports to
//! the service's event loop:
//!
//! - the main channel (role `peer`): the host's state, alerts and errors;
//! - the host channel (`upkeep.rs`): its stats every few seconds, its
//!   program maintenance and separately enabled synchronization plugins;
//! - an upstream per hub client using the host, so attaching, frames, focus
//!   and replies reach that client alone, as from the hub's own service.

use std::io::{BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::config::HostConfig;
use crate::hosts::upkeep::{self, HubCtx};
use crate::hosts::wire::{Channel, Welcome};
use crate::hosts::{self, fed};
use crate::protocol::{write_msg, ClientMsg, ClientRole, HostStats, ServerMsg, PROTOCOL_VERSION};
use crate::service::Ev;

/// The longest wait between attempts to reach a host.
const RETRY_MAX: Duration = Duration::from_secs(15);

pub enum HostEv {
    /// The main channel is open; `out` takes encoded messages for it.
    Up {
        tag: u16,
        out: Sender<Vec<u8>>,
        welcome: Welcome,
    },
    /// A message on the main channel, as the host sent it.
    Msg {
        tag: u16,
        msg: ServerMsg,
    },
    /// The main channel could not open or closed, and why.
    Down {
        tag: u16,
        reason: String,
    },
    Stats {
        tag: u16,
        stats: HostStats,
    },
    /// A message for hub client `cid` on its upstream, ids already the hub's.
    UpMsg {
        cid: u64,
        tag: u16,
        msg: ServerMsg,
    },
    /// A frame for hub client `cid`, encoded with the hub's id.
    UpFrame {
        cid: u64,
        tag: u16,
        buf: Vec<u8>,
    },
    /// The upstream could not open or closed.
    UpGone {
        cid: u64,
        tag: u16,
        reason: String,
    },
    /// Something about the host's program, repository mirror or sign-ins the owner
    /// should know, said once.
    Report {
        tag: u16,
        text: String,
    },
    /// A sign-in report that no longer holds, to take back from the list.
    Withdraw {
        tag: u16,
        text: String,
    },
}

pub fn report(tx: &Sender<Ev>, ev: HostEv) -> bool {
    tx.send(Ev::Host(Box::new(ev))).is_ok()
}

/// One line for the deck: the first cause only, never the whole chain.
pub(crate) fn short(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    crate::util::truncate_width(&crate::util::one_line(&text), 120)
}

pub fn pause(stop: &AtomicBool, wait: Duration) {
    let mut left = wait;
    while !stop.load(Ordering::SeqCst) && !left.is_zero() {
        let step = left.min(Duration::from_millis(200));
        thread::sleep(step);
        left -= step;
    }
}

/// Keep the main channel and the host channel to `host` open until `stop`.
/// A build host gets the host channel only: it runs no sessions.
pub fn spawn(host: HostConfig, key: String, tx: Sender<Ev>, stop: Arc<AtomicBool>, hub: Arc<HubCtx>) {
    if host.runs_agents() {
        let (h, k, t, s) = (host.clone(), key.clone(), tx.clone(), Arc::clone(&stop));
        let _ = thread::Builder::new().name(format!("host-{}", host.name)).spawn(move || main_loop(&h, &k, &t, &s));
    }
    let _ = thread::Builder::new()
        .name(format!("host-upkeep-{}", host.name))
        .spawn(move || upkeep::run(&host, &key, &tx, &stop, &hub));
}

fn main_loop(host: &HostConfig, key: &str, tx: &Sender<Ev>, stop: &AtomicBool) {
    let mut wait = Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        let reason = match hosts::open(host, key, Channel::Deck) {
            Ok((stream, welcome)) => {
                wait = Duration::from_secs(1);
                serve_main(host.tag, stream, welcome, tx)
            }
            Err(err) => short(&err),
        };
        if !report(tx, HostEv::Down { tag: host.tag, reason }) {
            return;
        }
        pause(stop, wait);
        wait = (wait * 2).min(RETRY_MAX);
    }
}

fn serve_main(tag: u16, stream: UnixStream, welcome: Welcome, tx: &Sender<Ev>) -> String {
    let Ok(mut writer) = stream.try_clone() else { return "could not use the connection".into() };
    // Keep using the host while upkeep brings it up to this build. Sending
    // our newer protocol first makes its service report a spurious error.
    let version = welcome.version.min(PROTOCOL_VERSION);
    if write_msg(&mut writer, &ClientMsg::Hello { version, role: ClientRole::Peer }).is_err() {
        return "the connection closed".into();
    }
    let out = writer_thread(writer);
    if !report(tx, HostEv::Up { tag, out, welcome }) {
        return String::new();
    }
    let mut reader = BufReader::new(stream);
    loop {
        match hosts::read_raw(&mut reader) {
            Ok(Some(body)) => {
                if fed::kind_of(&body) == Some("frame") {
                    continue;
                }
                // A newer host may send what this build cannot read.
                if let Ok(msg) = serde_json::from_slice::<ServerMsg>(&body) {
                    if !report(tx, HostEv::Msg { tag, msg }) {
                        return String::new();
                    }
                }
            }
            Ok(None) => return "the connection closed".into(),
            Err(err) => return format!("the connection failed: {err}"),
        }
    }
}

/// Write what arrives on the returned sender until it is dropped, then
/// close the socket, which ends its reader too.
fn writer_thread(mut writer: UnixStream) -> Sender<Vec<u8>> {
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let _ = thread::Builder::new().name("host-w".into()).spawn(move || {
        for buf in out_rx {
            if writer.write_all(&buf).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(Shutdown::Both);
    });
    out_tx
}

/// Open hub client `cid`'s own deck connection to `host`, speaking as
/// the originating client's role and negotiated protocol. Messages sent before
/// it is open wait in the returned sender;
/// dropping the sender closes it.
pub fn upstream(cid: u64, host: HostConfig, key: String, client: (ClientRole, u32), tx: Sender<Ev>) -> Sender<Vec<u8>> {
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let _ = thread::Builder::new().name(format!("up-{cid}-{}", host.name)).spawn(move || {
        let tag = host.tag;
        let (stream, welcome) = match hosts::open(&host, &key, Channel::Deck) {
            Ok(opened) => opened,
            Err(err) => {
                report(&tx, HostEv::UpGone { cid, tag, reason: short(&err) });
                return;
            }
        };
        let reason = serve_upstream((cid, tag), stream, welcome, client, &tx, out_rx);
        report(&tx, HostEv::UpGone { cid, tag, reason });
    });
    out_tx
}

fn serve_upstream(
    endpoint: (u64, u16),
    stream: UnixStream,
    welcome: Welcome,
    client: (ClientRole, u32),
    tx: &Sender<Ev>,
    out_rx: mpsc::Receiver<Vec<u8>>,
) -> String {
    let (cid, tag) = endpoint;
    let (role, client_version) = client;
    let Ok(mut writer) = stream.try_clone() else { return "could not use the connection".into() };
    // Source frame semantics must match the originating view too: an
    // older view still uses legacy Scroll even behind a newer hub.
    let version = welcome.version.min(PROTOCOL_VERSION).min(client_version);
    if write_msg(&mut writer, &ClientMsg::Hello { version, role }).is_err() {
        return "the connection closed".into();
    }
    let replies = tx.clone();
    let _ = thread::Builder::new().name("up-w".into()).spawn(move || {
        for buf in out_rx {
            // Check this actual connection, since a source may reconnect on an
            // older binary after the hub cached its capability advertisement.
            if version < 21 && buf.get(4..).and_then(fed::kind_of) == Some("plugin") {
                if let Ok(ClientMsg::Plugin { request, plugin, command, args }) = serde_json::from_slice(&buf[4..]) {
                    if plugin.starts_with("provider-") && command == "target" && args["operation"] == "send" && args["client_id"].is_string() {
                        report(&replies, HostEv::UpMsg { cid, tag, msg: ServerMsg::PluginResult {
                            request, data: serde_json::Value::Null,
                            error: Some("Update needed: source connection must support protocol 21 steering receipts; nothing was sent".into()),
                        }});
                        continue;
                    }
                }
            }
            // A host can reconnect on an older build after the main channel's
            // cached Welcome. Gate against this connection's actual handshake.
            if version < crate::protocol::NATIVE_OWNER_MESSAGES_PROTOCOL
                && matches!(
                    buf.get(4..).and_then(fed::kind_of),
                    Some("send_message" | "send_now" | "broadcast" | "broadcast_part" | "plugin" | "create")
                )
            {
                if let Ok(message) = serde_json::from_slice::<ClientMsg>(&buf[4..]) {
                    let error = "Update needed: source connection must support protocol 19 native owner messages; nothing was sent";
                    let reply = match message {
                        ClientMsg::Create { tool: crate::protocol::Tool::Codex, .. } => Some(ServerMsg::Notice { text: error.into(), error: true }),
                        ClientMsg::SendMessage { id, request, .. } => {
                            Some(ServerMsg::MessageSent { id, request, error: Some(error.into()), delivered: None })
                        }
                        ClientMsg::SendNow { id } => Some(ServerMsg::SentNow { id, error: Some(error.into()) }),
                        ClientMsg::BroadcastPart { request, ids, .. } => Some(ServerMsg::BroadcastDone {
                            request,
                            sent: vec![],
                            failed: ids.unwrap_or_else(|| vec![0]).into_iter().map(|id| (id, error.into())).collect(),
                        }),
                        ClientMsg::Broadcast { .. } => Some(ServerMsg::Notice { text: error.into(), error: true }),
                        ClientMsg::Plugin { request, plugin, command, args }
                            if plugin.starts_with("provider-")
                                && ((command == "target"
                                    && matches!(args["operation"].as_str(), Some("send" | "broadcast")))
                                    || (plugin == "provider-codex" && command == "control")
                                    || (plugin == "provider-dot" && command == "send")) =>
                        {
                            Some(ServerMsg::PluginResult { request, data: serde_json::Value::Null, error: Some(error.into()) })
                        }
                        _ => None,
                    };
                    if let Some(msg) = reply {
                        report(&replies, HostEv::UpMsg { cid, tag, msg });
                        continue;
                    }
                }
            }
            if version < crate::screen::HISTORY_PROTOCOL && buf.get(4..).and_then(fed::kind_of) == Some("history") {
                if let Ok(ClientMsg::History { id, request, epoch, cols, rows }) = serde_json::from_slice(&buf[4..]) {
                    report(
                        &replies,
                        HostEv::UpMsg {
                            cid,
                            tag,
                            msg: ServerMsg::History {
                                id,
                                request,
                                epoch,
                                cols,
                                rows,
                                snapshot: None,
                                error: Some(
                                    "The source connection does not support local history (protocol 18).".into(),
                                ),
                            },
                        },
                    );
                }
                continue;
            }
            // A source older than the native Stop gets the Esc it took before.
            if version < crate::protocol::NATIVE_STOP_PROTOCOL && buf.get(4..).and_then(fed::kind_of) == Some("stop") {
                if let Ok(ClientMsg::Stop { id }) = serde_json::from_slice(&buf[4..]) {
                    let esc = crate::protocol::encode(&ClientMsg::Input { id, data: "\x1b".into() });
                    if writer.write_all(&esc).is_err() {
                        break;
                    }
                }
                continue;
            }
            if writer.write_all(&buf).is_err() {
                break;
            }
        }
        let _ = writer.shutdown(Shutdown::Both);
    });
    let mut reader = BufReader::new(stream);
    loop {
        match hosts::read_raw(&mut reader) {
            Ok(Some(body)) => {
                let ev = match fed::kind_of(&body) {
                    // The main channel has the state; frames are only
                    // renumbered, never parsed.
                    Some("state" | "state_patch" | "welcome" | "alert") => continue,
                    Some("frame") => match fed::rewrite_frame(&body, tag) {
                        Some(buf) => HostEv::UpFrame { cid, tag, buf },
                        None => continue,
                    },
                    _ => match serde_json::from_slice::<ServerMsg>(&body) {
                        Ok(msg) => HostEv::UpMsg { cid, tag, msg },
                        Err(_) => continue,
                    },
                };
                if !report(tx, ev) {
                    return String::new();
                }
            }
            Ok(None) => break String::new(),
            Err(err) => break format!("the connection failed: {err}"),
        }
    }
}

#[cfg(test)]
#[path = "../tests/hosts_link.rs"]
mod tests;
