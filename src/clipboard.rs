//! The clipboard of the machine the deck runs on.
//!
//! A program reads the clipboard of the machine it runs on: Claude Code's
//! Ctrl+V runs `wl-paste` or `xclip` there, and a host's desktop session
//! hands its display to the deck service a host runs, so Ctrl+V in a
//! remote session would paste the remote clipboard. For a session on another
//! host the deck reads its own clipboard instead and sends the text, or the
//! image as a file whose path it pastes (`ClientMsg::PasteFile`).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// What the clipboard holds.
#[derive(Debug, PartialEq, Eq)]
pub enum Clip {
    Text(String),
    /// An image, named with its type's extension.
    Image {
        name: String,
        bytes: Vec<u8>,
    },
    Empty,
}

/// Image types in the order they are preferred when the clipboard offers
/// several, with the extension the kept file gets.
const IMAGES: [(&str, &str); 5] =
    [("image/png", "png"), ("image/jpeg", "jpg"), ("image/webp", "webp"), ("image/gif", "gif"), ("image/bmp", "bmp")];

/// Text types in the order they are preferred.
const TEXTS: [&str; 5] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"];

/// A clipboard read may not hold up the deck longer than this.
const TIMEOUT: Duration = Duration::from_secs(2);

/// The most a clipboard read keeps, the most a file the phone attaches may
/// hold as well (`remote::uploads::FILE_MAX`).
const MAX: u64 = crate::remote::uploads::FILE_MAX;

/// How the clipboard is read: `wl-paste` under Wayland, else `xclip`.
pub struct Reader {
    pub wayland: bool,
    pub wl_paste: String,
    pub xclip: String,
}

impl Reader {
    /// This machine's clipboard, through the display the deck runs under.
    pub fn system() -> Reader {
        Reader {
            wayland: std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty()),
            wl_paste: "wl-paste".into(),
            xclip: "xclip".into(),
        }
    }

    /// The clipboard's image if it has one, else its text.
    pub fn read(&self) -> Result<Clip> {
        // An empty clipboard makes both tools fail; so does a missing tool,
        // which `run` reports.
        let Some(listed) = self.types()? else { return Ok(Clip::Empty) };
        let types: Vec<&str> = listed.lines().map(str::trim).filter(|t| !t.is_empty()).collect();
        let image =
            IMAGES.iter().find(|(mime, _)| types.contains(mime)).copied().or_else(|| {
                types.iter().find(|t| t.starts_with("image/")).map(|t| (*t, t.trim_start_matches("image/")))
            });
        if let Some((mime, ext)) = image {
            let bytes = self.content(mime)?.unwrap_or_default();
            if bytes.is_empty() {
                return Ok(Clip::Empty);
            }
            let ext: String = ext.chars().filter(char::is_ascii_alphanumeric).take(8).collect();
            return Ok(Clip::Image { name: format!("clipboard-{}.{ext}", crate::util::now_secs()), bytes });
        }
        let text = TEXTS
            .iter()
            .find(|t| types.contains(t))
            .copied()
            .or_else(|| types.iter().find(|t| t.starts_with("text/")).copied());
        let Some(mime) = text else { return Ok(Clip::Empty) };
        let text = String::from_utf8_lossy(&self.content(mime)?.unwrap_or_default()).into_owned();
        Ok(if text.is_empty() { Clip::Empty } else { Clip::Text(text) })
    }

    /// The types the clipboard offers, one a line; None when it is empty.
    fn types(&self) -> Result<Option<String>> {
        let out = if self.wayland {
            run(&self.wl_paste, &["--list-types"])?
        } else {
            run(&self.xclip, &["-selection", "clipboard", "-t", "TARGETS", "-o"])?
        };
        Ok(out.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    fn content(&self, mime: &str) -> Result<Option<Vec<u8>>> {
        if self.wayland {
            run(&self.wl_paste, &["--no-newline", "--type", mime])
        } else {
            run(&self.xclip, &["-selection", "clipboard", "-t", mime, "-o"])
        }
    }
}

/// What `program` prints, None when it fails (an empty clipboard), or an
/// error when it cannot start, takes too long or prints too much.
fn run(program: &str, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("could not run {program}"))?;
    let stdout = child.stdout.take().context("no output")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.take(MAX + 1).read_to_end(&mut bytes);
        bytes
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if started.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{program} did not answer within {} seconds", TIMEOUT.as_secs());
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    if bytes.len() as u64 > MAX {
        bail!("the clipboard holds more than {} MiB", MAX >> 20);
    }
    Ok(status.success().then_some(bytes))
}

#[cfg(test)]
#[path = "tests/clipboard.rs"]
mod tests;
