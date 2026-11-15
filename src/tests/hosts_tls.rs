use super::*;
use crate::protocol::{read_msg, write_msg};
use crate::remote::store;
use crate::testsupport::TempDir;

use std::net::TcpListener;

/// A peer listener on loopback that answers each message with the same
/// message, as the bridge carries it.
fn echo_server(dir: &TempDir) -> (String, String) {
    let identity = store::load_or_create_identity(dir.path(), "127.0.0.1").unwrap();
    let config = crate::remote::server::tls_config(&identity.cert_der, &identity.key_der).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let config = Arc::clone(&config);
            thread::spawn(move || {
                let Ok((conn, tcp)) = accept(&config, tcp) else { return };
                let mut local = bridged(conn, tcp).unwrap();
                while let Ok(Some(text)) = read_msg::<_, String>(&mut local) {
                    if write_msg(&mut local, &text).is_err() {
                        break;
                    }
                }
            });
        }
    });
    (addr, identity.fingerprint)
}

#[test]
fn messages_cross_the_bridge_both_ways() {
    let dir = TempDir::new("tls-echo");
    let (addr, fingerprint) = echo_server(&dir);
    let (conn, tcp) = connect(&[addr], &fingerprint).unwrap();
    let mut local = bridged(conn, tcp).unwrap();
    // Larger than one TLS record and one socket read.
    let big = "x".repeat(300_000);
    for text in ["hello", big.as_str()] {
        write_msg(&mut local, &text.to_string()).unwrap();
        let back: String = read_msg(&mut local).unwrap().unwrap();
        assert_eq!(back, text);
    }
}

/// The hub sends its hello the moment its handshake is done, so the host can
/// read it in the same packet as the handshake's end, before its bridge
/// starts. On 2026-10-31 two of three deck channels to Fedora lost their
/// hello that way: "no answer to the hello" and "the host closed the
/// connection".
#[test]
fn a_message_arriving_with_the_end_of_the_handshake_crosses_the_bridge() {
    let dir = TempDir::new("tls-coalesced");
    let (addr, fingerprint) = echo_server(&dir);
    let mut tcp = TcpStream::connect(&addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let name = ServerName::try_from("cepheus").unwrap();
    let client = ClientConnection::new(client_config(&fingerprint).unwrap(), name).unwrap();
    let mut conn = Connection::Client(client);
    // The handshake by hand, holding back its last flight to send with the
    // first message.
    while conn.is_handshaking() {
        while conn.wants_write() {
            conn.write_tls(&mut tcp).unwrap();
        }
        conn.read_tls(&mut tcp).unwrap();
        conn.process_new_packets().unwrap();
    }
    let mut framed = Vec::new();
    write_msg(&mut framed, &"sent with the handshake".to_string()).unwrap();
    conn.writer().write_all(&framed).unwrap();
    flush_tls(&mut conn, &mut tcp).unwrap();
    let mut local = bridged(conn, tcp).unwrap();
    local.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    assert_eq!(read_msg::<_, String>(&mut local).unwrap().unwrap(), "sent with the handshake");
}

#[test]
fn a_certificate_other_than_the_pinned_one_is_refused() {
    let dir = TempDir::new("tls-pin");
    let (addr, fingerprint) = echo_server(&dir);
    let wrong: String = fingerprint.chars().rev().collect();
    let err = connect(&[addr], &wrong).expect_err("a different fingerprint must not connect");
    assert!(format!("{err:#}").contains("not the one recorded"), "{err:#}");
}

#[test]
fn the_next_address_is_tried_when_one_does_not_answer() {
    let dir = TempDir::new("tls-fallback");
    let (addr, fingerprint) = echo_server(&dir);
    // A port nothing listens on: bound, then closed.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    let (conn, tcp) = connect(&[closed, addr], &fingerprint).unwrap();
    let mut local = bridged(conn, tcp).unwrap();
    write_msg(&mut local, &"after a refusal".to_string()).unwrap();
    assert_eq!(read_msg::<_, String>(&mut local).unwrap().unwrap(), "after a refusal");
}
