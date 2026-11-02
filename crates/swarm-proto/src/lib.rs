//! Wire protocol shared by `swarmd`, `swarm-shim` and clients.
//!
//! Every connection is a byte stream of frames: a big-endian `u32` length
//! followed by a postcard-encoded message. Clients talk to `swarmd` with
//! [`ClientMsg`] / [`ServerMsg`]; `swarmd` talks to each shim with
//! [`ShimRequest`], [`ShimInput`] and [`ShimMsg`].

pub mod frame;
pub mod paths;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use frame::{read_frame, write_frame, FrameReader, MAX_FRAME_LEN};

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u32 = 1;

/// Hash of the swarm crates' sources this was built from.
pub const BUILD_ID: &str = env!("SWARM_BUILD_ID");

/// `<package version>+<build id>`: what `swarmd --version` prints and what the
/// daemon sends in `Welcome`. Clients compare it with their own to tell when
/// a running daemon is from another build.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("SWARM_BUILD_ID"));

/// Session ids are ULID strings, stable across daemon restarts.
pub type SessionId = String;

/// Identifies one attachment on a client connection.
pub type StreamId = u32;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Default for Size {
    fn default() -> Self {
        Size { cols: 80, rows: 24 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    Running,
    /// `code` is `None` when the process was killed by a signal.
    Exited { code: Option<i32>, signal: Option<i32>, at_ms: u64 },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ClientDesc {
    pub client_id: String,
    pub name: String,
    pub readonly: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: Option<String>,
    /// Last title set through OSC 0/2.
    pub title: String,
    /// Last cwd reported through OSC 7.
    pub cwd: Option<String>,
    /// argv of the session's main process.
    pub command: Vec<String>,
    pub pid: u32,
    /// Name of the terminal's foreground process, if it isn't the main process.
    pub foreground: Option<String>,
    pub state: SessionState,
    pub created_ms: u64,
    pub last_output_ms: u64,
    pub last_input_ms: u64,
    pub size: Size,
    pub clients: Vec<ClientDesc>,
    /// Opaque UI metadata (tab order, color, auto_attach, …) owned by clients.
    pub ui: BTreeMap<String, String>,
    /// Byte offsets of the output still held in the scrollback buffer.
    pub buffer_start: u64,
    pub buffer_end: u64,
    pub alt_screen: bool,
}

impl SessionInfo {
    pub fn display_name(&self) -> &str {
        if let Some(name) = &self.name {
            return name;
        }
        if !self.title.is_empty() {
            return &self.title;
        }
        self.command.first().map(|s| s.as_str()).unwrap_or("session")
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, SessionState::Running)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateOpts {
    /// argv to run; empty means the user's login shell.
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub size: Size,
    pub name: Option<String>,
    pub ui: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachMode {
    /// Replay from the oldest safe boundary still buffered, then live output.
    Full,
    /// Send output from this offset on (after a dropped connection); falls
    /// back to `Full` if the offset is no longer buffered.
    ResumeFrom(u64),
    /// Live output only.
    Live,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct MetaUpdate {
    /// `Some(None)` clears the name.
    pub name: Option<Option<String>>,
    /// Keys to set; an empty value removes the key.
    pub ui: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Client <-> swarmd

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Request {
    Ping,
    List,
    Create(CreateOpts),
    Kill { id: SessionId, signal: i32 },
    /// Kills the session if needed and drops it (and its buffer) for good.
    Remove { id: SessionId },
    SetMeta { id: SessionId, update: MetaUpdate },
    Attach { id: SessionId, mode: AttachMode, size: Option<Size>, readonly: bool },
    /// Start receiving [`SessionEvent`]s on this connection.
    Subscribe,
    /// Stop the daemon. Sessions keep running and are re-adopted on restart.
    Shutdown,
    /// Moves running sessions to this daemon's `swarm-shim` without
    /// restarting them; `force` also redoes shims already on this build.
    UpgradeShims { force: bool },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Response {
    Ok,
    Pong,
    Sessions(Vec<SessionInfo>),
    Session(SessionInfo),
    Attached {
        stream: StreamId,
        info: SessionInfo,
        /// First offset that will be sent.
        replay_from: u64,
        /// Output at or after this offset is live; earlier output is replay.
        live_from: u64,
    },
    ShimsUpgraded(Vec<ShimUpgrade>),
}

/// What `UpgradeShims` did with one session.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ShimUpgrade {
    pub id: SessionId,
    pub outcome: ShimUpgradeOutcome,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ShimUpgradeOutcome {
    /// Its shim now runs the daemon's build; the session kept running.
    Upgraded,
    /// Its shim already runs the daemon's build.
    UpToDate,
    /// Left alone, and why (it has exited, or its shim can't be updated in place).
    Skipped(String),
    /// The update didn't happen; the session keeps running on its old shim.
    Failed(String),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ClientMsg {
    Hello { proto: u32, client_name: String, client_id: String },
    Request { id: u64, req: Request },
    Input { stream: StreamId, data: Vec<u8> },
    Resize { stream: StreamId, size: Size },
    Detach { stream: StreamId },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SessionEvent {
    Created(SessionInfo),
    Updated(SessionInfo),
    Removed { id: SessionId },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ServerMsg {
    Welcome { proto: u32, daemon_version: String, hostname: String, os: String },
    Response { id: u64, result: Result<Response, String> },
    Output { stream: StreamId, offset: u64, data: Vec<u8>, replay: bool },
    Exited { stream: StreamId, code: Option<i32>, signal: Option<i32> },
    /// The client fell too far behind; it must re-attach with `ResumeFrom(resume_at)`.
    Lagged { stream: StreamId, resume_at: u64 },
    StreamClosed { stream: StreamId, reason: String },
    Event(SessionEvent),
}

// ---------------------------------------------------------------------------
// swarmd <-> shim. Each request opens its own connection to the shim.

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ShimRequest {
    Info,
    /// The shim pushes `Changed` whenever the session's info changes.
    Watch,
    /// After `Attached`, the connection carries `ShimInput` one way and
    /// output messages the other.
    Attach { client: ClientDesc, mode: AttachMode, size: Option<Size> },
    Kill { signal: i32 },
    Remove,
    SetMeta(MetaUpdate),
    /// Keep only the last `keep` bytes of scrollback (memory is short).
    Trim { keep: u64 },
    /// Which build the shim runs: answered with `ShimMsg::Version`.
    Version,
    /// Replace the shim's program with `exe`, keeping the session (same
    /// process, PTY, child and scrollback). Answered `Ok` once `exe` has
    /// agreed to take the session over, just before the switch.
    Upgrade { exe: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ShimInput {
    Input(Vec<u8>),
    Resize(Size),
    Detach,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ShimMsg {
    Ok,
    Err(String),
    Info(SessionInfo),
    Changed(SessionInfo),
    Attached { info: SessionInfo, replay_from: u64, live_from: u64 },
    Output { offset: u64, data: Vec<u8>, replay: bool },
    Exited { code: Option<i32>, signal: Option<i32> },
    Lagged { resume_at: u64 },
    /// `build_id` of the running shim program, the handoff format it reads
    /// and writes, and how many times it has been replaced in place.
    Version { build_id: String, handoff: u32, generation: u32 },
}

/// Hex encoding, used to pass `CreateOpts` to a shim on its command line.
pub mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn decode(s: &str) -> Option<Vec<u8>> {
        if !s.len().is_multiple_of(2) {
            return None;
        }
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
