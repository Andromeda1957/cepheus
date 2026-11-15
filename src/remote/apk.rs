//! The phone app's download, which the owner requires to stay available:
//! plain HTTP at `apk_listen` on the VPN address, serving the files of
//! `~/.local/share/agent-deck/apk/` and a page listing them, as the
//! `python3 -m http.server` unit it replaces did. A thread of the gateway:
//! each request is answered on a thread of its own and the file is sent by
//! the kernel.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Requests answered at once; more are closed unanswered.
const MAX_CONNS: usize = 16;
/// The request line and headers, which nothing here reads past.
const HEAD_LIMIT: u64 = 8 * 1024;

/// The folder the release build publishes the APK to, which keeps its name
/// from before the rename to Cepheus.
pub fn dir() -> PathBuf {
    crate::paths::home().join(".local/share/agent-deck/apk") // legacy-name
}

/// Listen at `listen`, retrying every ten seconds while the address is not
/// up yet (the VPN starting after the gateway), and answer forever.
pub fn serve(listen: String, dir: PathBuf) {
    let mut reported = false;
    loop {
        match TcpListener::bind(&listen) {
            Ok(listener) => {
                eprintln!("cepheus remote: serving the APK download on {listen}");
                accept_all(listener, &dir);
            }
            Err(err) => {
                if !reported {
                    eprintln!("cepheus remote: cannot serve the APK download on {listen} yet: {err}");
                    reported = true;
                }
                thread::sleep(Duration::from_secs(10));
            }
        }
    }
}

pub fn accept_all(listener: TcpListener, dir: &Path) {
    let open = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if open.load(Ordering::Relaxed) >= MAX_CONNS {
            continue;
        }
        open.fetch_add(1, Ordering::Relaxed);
        let (open, dir) = (Arc::clone(&open), dir.to_path_buf());
        thread::spawn(move || {
            // A phone that leaves mid-download only ends its own request.
            let _ = answer(stream, &dir);
            open.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

fn answer(mut stream: TcpStream, dir: &Path) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut head = BufReader::new((&stream).take(HEAD_LIMIT));
    let mut line = String::new();
    head.read_line(&mut line)?;
    loop {
        let mut header = String::new();
        if head.read_line(&mut header)? == 0 || header.trim_end_matches(['\r', '\n']).is_empty() {
            break;
        }
    }
    let mut words = line.split_whitespace();
    let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    let body_too = match method {
        "GET" => true,
        "HEAD" => false,
        _ => return reply(&mut stream, "501 Not Implemented", TEXT, b"Unsupported method\n", true),
    };
    let path = target.split(['?', '#']).next().unwrap_or("");
    if path == "/" {
        return reply(&mut stream, "200 OK", "text/html; charset=utf-8", index(dir).as_bytes(), body_too);
    }
    let Some((file, name)) = path.strip_prefix('/').and_then(name).and_then(|name| Some((open(dir, &name)?, name)))
    else {
        return reply(&mut stream, "404 Not Found", TEXT, b"File not found\n", body_too);
    };
    // The length of the file opened, which a replacement published while
    // it is sent does not change.
    let len = file.metadata()?.len();
    let kind =
        if name.ends_with(".apk") { "application/vnd.android.package-archive" } else { "application/octet-stream" };
    head_lines(&mut stream, "200 OK", kind, len)?;
    if body_too {
        io::copy(&mut (&file).take(len), &mut stream)?;
    }
    stream.flush()
}

const TEXT: &str = "text/plain; charset=utf-8";

fn head_lines(stream: &mut TcpStream, status: &str, kind: &str, len: u64) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.0 {status}\r\nServer: cepheus\r\nContent-Type: {kind}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
    )
}

fn reply(stream: &mut TcpStream, status: &str, kind: &str, body: &[u8], body_too: bool) -> io::Result<()> {
    head_lines(stream, status, kind, body.len() as u64)?;
    if body_too {
        stream.write_all(body)?;
    }
    stream.flush()
}

/// The page at `/`: a link to each file, as the Python server listed them.
pub fn index(dir: &Path) -> String {
    let mut page = String::from(
        "<!DOCTYPE HTML>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Directory listing for /</title>\n\
         </head>\n<body>\n<h1>Directory listing for /</h1>\n<hr>\n<ul>\n",
    );
    for name in files(dir) {
        let shown = escape(&name);
        let _ = writeln!(page, "<li><a href=\"{}\">{shown}</a></li>", encode(&name));
    }
    page.push_str("</ul>\n<hr>\n</body>\n</html>\n");
    page
}

/// The regular files of `dir` by name, without staging files (a leading
/// dot), sorted.
fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// The file a request path names: one percent-decoded segment, never a
/// staging file or a way out of the folder.
pub fn name(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))?;
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let name = String::from_utf8(out).ok()?;
    (!name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\0'])).then_some(name)
}

/// A regular file directly in `dir`, never one a link leads to.
fn open(dir: &Path, name: &str) -> Option<fs::File> {
    let path = dir.join(name);
    if !fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    fs::File::open(path).ok()
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for &byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
#[path = "../tests/remote_apk.rs"]
mod tests;
