//! Bounded text snapshots from a configured local provider, without executable content.
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::remote::chat::{clip, Item, TEXT_MAX};
use serde::Deserialize;

const BYTES_MAX: u64 = 8 << 20;
const ITEMS_MAX: usize = 3000;

#[derive(Deserialize)]
#[serde(tag = "k", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    User {
        text: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        at: Option<u64>,
    },
    Assistant {
        text: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        at: Option<u64>,
    },
    Notice {
        text: String,
    },
}

/// Only plain messages are accepted. Symlinks, special files, oversized data,
/// and unrelated JSON (including browser state) never become phone chat.
pub fn read(path: &Path) -> Option<Vec<Item>> {
    identified(path).map(|(items, _)| items)
}

/// Stable provider identities let a native view reconcile a delivery receipt
/// with the same atomic snapshot rather than guessing from repeated wording.
pub fn identified(path: &Path) -> Option<(Vec<Item>, Vec<String>)> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > BYTES_MAX {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(BYTES_MAX + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > BYTES_MAX {
        return None;
    }
    let messages: Vec<Message> = serde_json::from_slice(&bytes).ok()?;
    if messages.len() > ITEMS_MAX {
        return None;
    }
    Some(
        messages
            .into_iter()
            .map(|message| match message {
                Message::User { text, at, id } => (
                    Item::User { client_id: None, text: clip(&text, TEXT_MAX), images: Vec::new(), at },
                    id.unwrap_or_default(),
                ),
                Message::Assistant { text, at, id } => {
                    (Item::Assistant { text: clip(&text, TEXT_MAX), questions: Vec::new(), at }, id.unwrap_or_default())
                }
                Message::Notice { text } => (Item::Notice { text: clip(&text, TEXT_MAX) }, String::new()),
            })
            .unzip(),
    )
}

/// Only the current Dot controller's recent private activity can advertise work.
/// Conversation and composer text never serve as an activity signal.
pub fn working(transcript: &Path, pid: Option<u32>, now_ms: u64) -> bool {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Activity {
        pid: u32,
        at: u64,
        known: bool,
        working: bool,
    }
    let Some(pid) = pid else { return false };
    let Some(directory) = transcript.parent() else { return false };
    let read = || -> Option<Activity> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join("activity.json"))
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > 1024 {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(1025).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 1024 {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    };
    read().is_some_and(|activity| {
        activity.pid == pid
            && activity.known
            && activity.working
            && activity.at <= now_ms
            && now_ms - activity.at <= 10_000
    })
}

/// Whether the controller's delivery journal, a private `pending.json` beside
/// the transcript, holds a message whose delivery has not settled. Only no
/// journal or exactly JSON `null` is settled; anything else, unreadable
/// included, is not, so nothing that could drop or resend a message counts
/// as quiet.
pub fn delivery_pending(transcript: &Path) -> bool {
    let Some(directory) = transcript.parent() else { return true };
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("pending.json"))
    {
        Ok(file) => file,
        Err(error) => return error.kind() != std::io::ErrorKind::NotFound,
    };
    let mut bytes = Vec::new();
    if !file.metadata().is_ok_and(|m| m.is_file() && m.len() <= BYTES_MAX)
        || file.take(BYTES_MAX + 1).read_to_end(&mut bytes).is_err()
    {
        return true;
    }
    !matches!(serde_json::from_slice(&bytes), Ok(serde_json::Value::Null))
}
