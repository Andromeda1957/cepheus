//! What the hub and a host's peer listener say to each other: a hello that
//! proves the hub and names the channel, then either the deck protocol
//! unchanged (`deck`) or host requests (`host`), each message
//! length-prefixed JSON as in `protocol.rs`.

use serde::{Deserialize, Serialize};

use crate::hosts::logins::{LoginState, LoginTool};
use crate::hosts::mirror::Ff;
use crate::protocol::{HostStats, SessionInfo, Tool};
use crate::remote::chat::Chunk;
use crate::remote::search::Found;

/// The peer conversation's own version, apart from the deck protocol's.
pub const PEER_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// A byte pipe to the host's own service socket.
    Deck,
    /// Requests for the host itself: stats, the repository mirror, sign-ins.
    Host,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Hello {
    pub v: u32,
    /// The hub's key, hex; the host keeps only its SHA-256.
    pub key: String,
    pub channel: Channel,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Welcome {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// The host's deck protocol (`PROTOCOL_VERSION`).
    #[serde(default)]
    pub version: u32,
    /// The host's own name for itself.
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub start_dir: String,
    /// SHA-256 of the host's program and when that file was made, so the
    /// hub can bring an older host up to its own build.
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub built: u64,
    /// Optional plugin capabilities; absent on peers predating negotiation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HostReq {
    /// A viewer reaches native phone content through its pinned coordinator.
    /// Only the fixed phone content operations are allowed, with at most one hop.
    PhoneForward {
        host: Option<String>,
        request: Box<HostReq>,
    },
    PhoneModels,
    PhoneStats,

    /// This physical interface's public key digest, over an authenticated channel.
    ViewerIdentity,
    /// Authorize another interface without transferring its private key.
    AuthorizeViewer {
        sha256: String,
    },
    /// Install the authenticated caller as this interface's pinned coordinator.
    ConfigureCoordinator {
        host: crate::config::HostConfig,
    },
    /// A bounded saved security report, by scan id.
    ScanReport {
        id: String,
    },
    /// CPU, memory and disk, and unless `resources_only` the start
    /// directory's branches and worktrees. A build host is asked for its
    /// resources only.
    Stats {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        resources_only: bool,
    },
    /// The API usage entries this host's collector recorded after `cursor`,
    /// for the collector instance `instance` names.
    ApiCost {
        instance: String,
        cursor: i64,
    },
    /// Replace the host's program with the `len` bytes that follow this
    /// message, whose SHA-256 is `sha256`, dated `built`.
    Update {
        len: u64,
        sha256: String,
        built: u64,
    },
    /// Fetch the hub's branches and tags from the remote this host's own
    /// `git-mirror` settings name, then fast-forward `master` when safe.
    MirrorFetch,
    /// Its Claude and Codex sign-ins, without their secrets.
    Logins,
    GetLogin {
        tool: LoginTool,
    },
    PutLogin {
        tool: LoginTool,
        login: serde_json::Value,
    },
    /// Explicit profile requests require protocol 14. IDs are resolved only
    /// through the receiving host's own configured account profiles.
    ProfileLogins,
    GetProfileLogin {
        profile: String,
        tool: LoginTool,
    },
    PutProfileLogin {
        profile: String,
        tool: LoginTool,
        login: serde_json::Value,
    },
    /// For the phone: what a copy of a session's chat lacks
    /// ([`crate::remote::chat::Cache::chunk`]). `session` carries the
    /// host's own id.
    Chat {
        session: Box<SessionInfo>,
        from: usize,
        generation: u64,
    },
    /// The skills and commands a session in `cwd` (its start directory
    /// when none) can run.
    Commands {
        tool: Tool,
        cwd: Option<String>,
    },
    /// A file the phone attached, `len` bytes following: kept for the
    /// session's message to name.
    PutFile {
        name: String,
        len: u64,
    },
    /// For the phone: which of these sessions (the host's own ids) hold
    /// every word of `words` in their conversation
    /// ([`crate::remote::search`]).
    Search {
        sessions: Vec<SessionInfo>,
        words: Vec<String>,
    },
    /// For the phone: an image the owner sent in a session (the host's own
    /// id), by its key ([`crate::remote::chat::image`]).
    Image {
        session: Box<SessionInfo>,
        key: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolLogin {
    pub tool: LoginTool,
    pub state: LoginState,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProfileLogin {
    pub profile: String,
    pub tool: LoginTool,
    pub state: LoginState,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HostResp {
    PhoneModels {
        tools: serde_json::Value,
    },

    ViewerIdentity {
        sha256: String,
    },
    ScanReport {
        text: String,
    },
    Stats {
        stats: HostStats,
    },
    ApiCost {
        export: crate::api_cost::Export,
    },
    Updated,
    Mirrored {
        ff: Ff,
    },
    Logins {
        logins: Vec<ToolLogin>,
    },
    ProfileLogins {
        logins: Vec<ProfileLogin>,
    },
    /// A sign-in, secrets and all: only ever sent over the TLS channel and
    /// never logged.
    Login {
        login: serde_json::Value,
    },
    Done,
    Error {
        text: String,
    },
    Chat {
        chunk: Chunk,
    },
    /// `remote::commands::Entry` values, as the phone reads them.
    Commands {
        entries: serde_json::Value,
    },
    /// Where a `PutFile` was kept.
    Put {
        path: String,
    },
    Search {
        found: Found,
    },
    /// An `Image`'s media type and base64 bytes.
    Image {
        media: String,
        data: String,
    },
}
