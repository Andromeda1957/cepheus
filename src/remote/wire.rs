//! The phone's JSON messages, one per WebSocket text frame (`README.md`,
//! "Phone"). Each request carries `rid`; the reply repeats it.

use serde::Deserialize;

use crate::protocol::Tool;

/// The first message of a connection.
#[derive(Deserialize, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Hello {
    /// A paired phone: its device id and key (hex), and the last alert it
    /// has, to be sent the ones after it.
    Auth {
        device: String,
        key: String,
        #[serde(default)]
        last_seq: Option<u64>,
        #[serde(default)]
        epoch: Option<String>,
    },
    /// A new phone with the code the deck's pairing screen shows.
    Pair { code: String, name: String },
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Req {
    /// Authenticated desktop controls; arguments are validated by action.
    Control {
        rid: u64,
        action: String,
        #[serde(flatten)]
        args: serde_json::Value,
    },
    /// Older chat items, before index `before`.
    Chat {
        rid: u64,
        id: u64,
        before: Option<usize>,
        limit: Option<usize>,
    },
    /// Follow one session's chat (or none): the reply is its latest page,
    /// and new items follow as `items`.
    Open {
        rid: u64,
        id: Option<u64>,
        limit: Option<usize>,
    },
    /// A message, and the uploads whose paths are typed after it.
    Send {
        rid: u64,
        id: u64,
        text: String,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        files: Vec<String>,
        /// A client that follows the fresh session its `/clear` starts: the
        /// host clears natively instead of taking `/clear` as a message.
        #[serde(default)]
        relocate: bool,
    },
    /// Begin a file for session `id`; its pieces follow as binary frames
    /// ([`piece_frame`]).
    Upload {
        rid: u64,
        id: u64,
        name: String,
        size: u64,
        sha256: String,
    },
    Answer {
        rid: u64,
        id: u64,
        inputs: u64,
        fp: String,
        choice: usize,
        /// For a several-choice question: the options to leave ticked.
        #[serde(default)]
        choices: Vec<usize>,
        text: Option<String>,
    },
    /// Answer one structured Codex question through its native editor.
    QuestionReply {
        rid: u64,
        id: u64,
        question: String,
        choice: Option<usize>,
        #[serde(default)]
        text: Option<String>,
    },
    Stop {
        rid: u64,
        id: u64,
    },
    /// Claude Code's send now: the messages queued behind the running turn
    /// reach the model now (`ClientMsg::SendNow`).
    SendNow {
        rid: u64,
        id: u64,
    },
    New {
        rid: u64,
        tool: Tool,
        model: Option<String>,
        effort: Option<String>,
        text: Option<String>,
        /// Another host the deck drives, by name; absent: this one.
        #[serde(default)]
        host: Option<String>,
    },
    Model {
        rid: u64,
        id: u64,
        model: Option<String>,
        effort: Option<String>,
    },
    Close {
        rid: u64,
        id: u64,
    },
    Reopen {
        rid: u64,
        id: u64,
    },
    /// The earlier conversations to resume, as the deck's `Alt+r` lists them.
    Conversations {
        rid: u64,
    },
    /// Resume an earlier conversation in a new session, or show the
    /// session already having it, as the deck's `Alt+r` does.
    Resume {
        rid: u64,
        tool: Tool,
        conversation: String,
        #[serde(default)]
        host: Option<String>,
    },
    ClearErrors {
        rid: u64,
    },
    Commands {
        rid: u64,
        id: u64,
    },
    Models {
        rid: u64,
    },
    /// Put the sessions in this order, as Super+↑/↓ does in the deck.
    Reorder {
        rid: u64,
        ids: Vec<u64>,
    },
    /// The agents' usage and this machine's CPU, memory and disk.
    Stats {
        rid: u64,
    },
    /// The sessions whose conversation holds every word of `q`
    /// ([`crate::remote::search`]).
    Search {
        rid: u64,
        q: String,
    },
    /// One image the owner sent in session `id`'s conversation, by the key
    /// its `user` item carries ([`crate::remote::chat::image`]).
    Image {
        rid: u64,
        id: u64,
        key: String,
    },
    Ping {
        rid: u64,
    },
}

impl Req {
    pub fn rid(&self) -> u64 {
        match self {
            Req::Control { rid, .. }
            | Req::Chat { rid, .. }
            | Req::Open { rid, .. }
            | Req::Send { rid, .. }
            | Req::Upload { rid, .. }
            | Req::Answer { rid, .. }
            | Req::QuestionReply { rid, .. }
            | Req::Stop { rid, .. }
            | Req::SendNow { rid, .. }
            | Req::New { rid, .. }
            | Req::Model { rid, .. }
            | Req::Close { rid, .. }
            | Req::Reopen { rid, .. }
            | Req::Conversations { rid }
            | Req::Resume { rid, .. }
            | Req::ClearErrors { rid }
            | Req::Commands { rid, .. }
            | Req::Models { rid }
            | Req::Reorder { rid, .. }
            | Req::Stats { rid }
            | Req::Search { rid, .. }
            | Req::Image { rid, .. }
            | Req::Ping { rid } => *rid,
        }
    }
}

/// Longest message the phone may send to a session.
pub const SEND_MAX: usize = 32 << 10;

/// The header of a piece of an upload.
#[derive(Deserialize, Debug, PartialEq)]
pub struct Piece {
    pub rid: u64,
    pub upload: String,
    pub offset: u64,
}

/// A binary frame: the header's length (4 bytes, big-endian), the header as
/// JSON, then the piece's bytes.
pub fn piece_frame(frame: &[u8]) -> Result<(Piece, &[u8]), String> {
    let len = frame.get(..4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or("a short frame")?;
    let header = frame.get(4..4 + len).ok_or("a short frame")?;
    let piece = serde_json::from_slice(header).map_err(|e| format!("bad piece header: {e}"))?;
    Ok((piece, &frame[4 + len..]))
}
