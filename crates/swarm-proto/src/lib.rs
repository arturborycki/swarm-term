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

pub use frame::{read_frame, read_frame_body, undecodable_request_id, write_frame, FrameReader, MAX_FRAME_LEN};

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

    /// Drops control and invisible characters from the text a program in the
    /// session sets (title, folder, foreground name) and from the name. A
    /// current shim already cleans them; an older one, on a remote host,
    /// may not.
    pub fn cleaned(mut self) -> Self {
        self.name = self.name.map(|n| clean_text(&n));
        self.title = clean_text(&self.title);
        self.cwd = self.cwd.map(|c| clean_text(&c));
        self.foreground = self.foreground.map(|f| clean_text(&f));
        self
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
    /// Everything still kept, oldest first: output spilled to disk, then the
    /// whole in-memory buffer (no trimming of long blocks), then live output.
    History,
}

/// Spill-to-disk: output that leaves a session's in-memory buffer is kept in
/// `dir/<session id>/`, encrypted with a key only the session's shim holds,
/// up to `limit` bytes (oldest dropped first).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SpillConfig {
    pub dir: String,
    pub limit: u64,
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
    /// Re-read the spill-to-disk setting and apply it to running sessions.
    ApplySpill,
    // New variants go last: older daemons decode requests by position.
    /// Type `data` into the session, as a client would, without attaching.
    Write { id: SessionId, data: Vec<u8> },
    /// The session's screen as it looks now.
    Screen { id: SessionId },
    /// What the programs in every session report about themselves.
    Status,
    /// Start receiving [`ServerMsg::Status`] on this connection.
    SubscribeStatus,
    /// The user has seen the session: drop its `done` and `error` records
    /// and its last notification.
    ClearStatus { id: SessionId },
    /// The TCP ports programs in each running session listen on, such as a
    /// development server started there. Answered with [`Response::Ports`].
    Ports,
    /// Look for text in every session's scrollback, spilled output included.
    /// Answered with [`Response::Found`].
    Search(SearchOpts),
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
    // New variants go last.
    Screen(ScreenCapture),
    Statuses(Vec<(SessionId, ProgramStatus)>),
    Ports(Vec<(SessionId, Vec<ListeningPort>)>),
    /// Sessions with a match, each with its most recent matching lines, and
    /// those that weren't searched.
    Found(SearchResults),
}

/// What [`Request::Search`] looks for.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SearchOpts {
    /// Plain text, not a pattern.
    pub text: String,
    /// `None`: only when `text` has a capital letter (smart case).
    pub case_sensitive: Option<bool>,
    /// Most matching lines kept per session, the most recent ones.
    pub max_hits: u32,
}

/// Most matching lines kept per session.
pub const MAX_SEARCH_HITS: u32 = 1000;
/// Longest search text, in bytes; longer text can't match a kept line anyway.
pub const MAX_SEARCH_TEXT: usize = 1024;

impl SearchOpts {
    pub fn is_case_sensitive(&self) -> bool {
        self.case_sensitive.unwrap_or_else(|| self.text.chars().any(char::is_uppercase))
    }

    /// Within the limits: an error for text too long to search for, and at
    /// most [`MAX_SEARCH_HITS`] lines per session.
    pub fn checked(mut self) -> Result<SearchOpts, String> {
        if self.text.len() > MAX_SEARCH_TEXT {
            return Err(format!("the text is longer than {MAX_SEARCH_TEXT} bytes"));
        }
        self.max_hits = self.max_hits.clamp(1, MAX_SEARCH_HITS);
        Ok(self)
    }
}

/// The answer to [`Request::Search`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResults {
    /// Sessions with a match.
    pub found: Vec<SessionHits>,
    /// Sessions that weren't searched (an older build, too slow, or the
    /// answer was too big), with why.
    pub skipped: Vec<(SessionId, String)>,
}

/// One line of output that matches.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    /// Where the line starts in the session's output.
    pub offset: u64,
    /// The line as text (escape sequences removed), shortened around the
    /// match when it's long.
    pub line: String,
    /// Where the match is in `line`, in characters: start and length.
    pub at: (u32, u32),
}

/// A session's matches, newest first.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SessionHits {
    pub id: SessionId,
    pub hits: Vec<SearchHit>,
    /// Matching lines in all, including those not kept.
    pub total: u64,
}

/// A TCP port a program in a session listens on.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ListeningPort {
    pub port: u16,
    /// The listening process.
    pub pid: u32,
    /// Its name ("node", "python3").
    pub program: String,
    /// Bound to 127.0.0.1 or ::1 only, so reachable from other machines
    /// only through a forward.
    pub loopback_only: bool,
}

/// A session's screen: the visible rows (no scrollback), as the shim's
/// terminal model has them.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ScreenCapture {
    pub size: Size,
    /// One string per row, trailing blanks removed.
    pub rows: Vec<String>,
    /// For each row, whether it continues on the next (the line was wrapped).
    pub wrapped: Vec<bool>,
    /// Escape sequences that draw the screen, colors included.
    pub formatted: Vec<u8>,
    /// Cursor (row, column), from zero.
    pub cursor: (u16, u16),
    pub alt_screen: bool,
    /// Whether the program asked for application cursor keys (DECCKM).
    pub application_cursor: bool,
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
    // New variants go last. Sent only after `Request::SubscribeStatus`.
    /// A session's program status changed.
    Status { id: SessionId, status: ProgramStatus },
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
    // New variants go last: older shims decode requests by position.
    /// Start (`Some`) or stop spilling output that leaves the in-memory
    /// buffer to disk. Stopping deletes what was spilled.
    SetSpill(Option<SpillConfig>),
    /// Type into the session without an attachment (`swarmctl send`).
    Write(Vec<u8>),
    /// Answered with `ShimMsg::Screen`.
    Screen,
    /// Answered with `ShimMsg::Status`.
    Status,
    /// Like `Status`, then another `ShimMsg::Status` on every change.
    WatchStatus,
    /// The user has seen the session (see `Request::ClearStatus`).
    ClearStatus,
    /// Answered with `ShimMsg::Found`.
    Search(SearchOpts),
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
    // New variants go last.
    Screen(ScreenCapture),
    Status(ProgramStatus),
    /// Matching lines, newest first, and how many matched in all.
    Found { hits: Vec<SearchHit>, total: u64 },
}

// ---------------------------------------------------------------------------
// Program status: what programs say about themselves through escape sequences
// (OSC 7501, the Program Status Protocol, and OSC 9;4 progress), plus bells,
// desktop notifications (OSC 9, 777, 99) and command exit codes (OSC 133;D).

// Lowercase names in JSON (`swarmctl --json`); postcard uses positions.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum StatusState {
    /// At rest, waiting for the user's next instruction.
    Idle,
    /// Running; may carry progress.
    Working,
    /// Finished; the result is ready and the user hasn't seen it yet.
    Done,
    /// Failed and stopped.
    Error,
    /// Can't continue until the user does something.
    Blocked,
}

impl StatusState {
    pub fn as_str(self) -> &'static str {
        match self {
            StatusState::Idle => "idle",
            StatusState::Working => "working",
            StatusState::Done => "done",
            StatusState::Error => "error",
            StatusState::Blocked => "blocked",
        }
    }
}

/// What a blocked program waits for.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BlockedKind {
    /// Approval to do something.
    Permission,
    /// An answer the user must type.
    Question,
    /// A login, token or credential.
    Auth,
}

impl BlockedKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockedKind::Permission => "permission",
            BlockedKind::Question => "question",
            BlockedKind::Auth => "auth",
        }
    }
}

/// One status record. The program's own is the root record (`id` empty);
/// it may report others, like `build/test`, for work it does in parallel.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StatusRecord {
    pub id: String,
    pub state: StatusState,
    /// Only with `Blocked`.
    pub kind: Option<BlockedKind>,
    /// 0 to 100, with `Working` or `Blocked`; `None` is indeterminate.
    pub progress: Option<u8>,
    /// Stable program identifier (`cargo`, `claude-code`), inherited from
    /// the nearest ancestor record that has one.
    pub app: Option<String>,
    /// Short label, for programs that report several records.
    pub title: Option<String>,
    /// One line: what it's doing, waiting for, or finished.
    pub msg: Option<String>,
    pub updated_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub at_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgramStatus {
    /// Sorted by id; the root record (if any) first.
    pub records: Vec<StatusRecord>,
    /// Bells rung since the session started, and when the last one rang.
    pub bells: u64,
    pub bell_ms: u64,
    /// The last desktop notification the program asked for, until the user
    /// has seen the session.
    pub notification: Option<Notification>,
    /// Exit code of the last command the shell finished (OSC 133;D), and when.
    pub last_exit: Option<(i32, u64)>,
    /// When any of this last changed.
    pub changed_ms: u64,
}

impl ProgramStatus {
    /// The record that most needs the user: blocked, then error, done,
    /// working, idle; the most recent among equals.
    pub fn headline(&self) -> Option<&StatusRecord> {
        self.records.iter().max_by_key(|r| (r.state, r.updated_ms))
    }

    /// `record`'s app, or the nearest ancestor's.
    pub fn app_of<'a>(&'a self, record: &'a StatusRecord) -> Option<&'a str> {
        let mut id = record.id.as_str();
        loop {
            if let Some(app) = self.records.iter().find(|r| r.id == id).and_then(|r| r.app.as_deref()) {
                return Some(app);
            }
            if id.is_empty() {
                return None;
            }
            id = id.rfind('/').map_or("", |i| &id[..i]);
        }
    }
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

/// Characters that must not reach a terminal from session output shown
/// elsewhere (titles, search results): control characters, which can start
/// escape sequences (C1 ones included), and invisible or direction-changing
/// characters, which make text read differently from what it is.
pub fn is_unsafe_char(c: char) -> bool {
    c.is_control() && c != '\t'
        || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}')
}

/// `text` without [`is_unsafe_char`] characters; a tab becomes a space.
impl ProgramStatus {
    /// `ProgramStatus` with the text programs report cleaned, as `SessionInfo::cleaned`.
    pub fn cleaned(mut self) -> Self {
        let opt = |t: Option<String>| t.map(|t| clean_text(&t));
        for r in &mut self.records {
            r.id = clean_text(&r.id);
            r.app = opt(r.app.take());
            r.title = opt(r.title.take());
            r.msg = opt(r.msg.take());
        }
        if let Some(n) = &mut self.notification {
            n.title = clean_text(&n.title);
            n.body = clean_text(&n.body);
        }
        self
    }
}

pub fn clean_text(text: &str) -> String {
    text.chars().filter(|&c| !is_unsafe_char(c)).map(|c| if c == '\t' { ' ' } else { c }).collect()
}

/// Live output with any `OSC 1337 ; SwarmTime=` a program sent made
/// harmless. Swarm sends that sequence itself around replayed output (the
/// times of replayed blocks, and the end of a replay); a program printing it
/// could otherwise fake both. The key is renamed, which terminals ignore. A
/// possible start at the end of a chunk is held for the next one, or
/// [`Self::finish`].
#[derive(Default)]
pub struct ForgedTimeFilter {
    carry: Vec<u8>,
}

const SWARM_TIME: &[u8] = b"\x1b]1337;SwarmTime=";

impl ForgedTimeFilter {
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let mut input = std::mem::take(&mut self.carry);
        input.extend_from_slice(data);
        let mut out = Vec::with_capacity(input.len());
        let mut i = 0;
        while i < input.len() {
            let rest = &input[i..];
            if rest.starts_with(SWARM_TIME) {
                out.extend_from_slice(b"\x1b]1337;SwarmTime-forged=");
                i += SWARM_TIME.len();
            } else if rest[0] == 0x1b && SWARM_TIME.starts_with(rest) {
                // Cut off at the end: maybe the start of one.
                self.carry = rest.to_vec();
                break;
            } else {
                out.push(rest[0]);
                i += 1;
            }
        }
        out
    }

    /// What was held back.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.carry)
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// postcard writes an enum variant as its position: older shims, daemons
    /// and apps decode by it, so a variant must never move. Add new ones last.
    #[test]
    fn variants_keep_their_positions() {
        let tag = |bytes: Vec<u8>| bytes[0];
        assert_eq!(tag(postcard::to_stdvec(&ShimRequest::Trim { keep: 0 }).unwrap()), 6);
        assert_eq!(tag(postcard::to_stdvec(&ShimRequest::Version).unwrap()), 7);
        assert_eq!(tag(postcard::to_stdvec(&ShimRequest::Upgrade { exe: String::new() }).unwrap()), 8);
        assert_eq!(tag(postcard::to_stdvec(&ShimRequest::SetSpill(None)).unwrap()), 9);
        assert_eq!(tag(postcard::to_stdvec(&Request::Shutdown).unwrap()), 8);
        assert_eq!(tag(postcard::to_stdvec(&Request::UpgradeShims { force: false }).unwrap()), 9);
        assert_eq!(tag(postcard::to_stdvec(&Request::ApplySpill).unwrap()), 10);
        assert_eq!(tag(postcard::to_stdvec(&Request::Ports).unwrap()), 16);
        assert_eq!(tag(postcard::to_stdvec(&ShimRequest::ClearStatus).unwrap()), 14);
        assert_eq!(tag(postcard::to_stdvec(&AttachMode::Live).unwrap()), 2);
        assert_eq!(tag(postcard::to_stdvec(&AttachMode::History).unwrap()), 3);
        assert_eq!(tag(postcard::to_stdvec(&ShimMsg::Lagged { resume_at: 0 }).unwrap()), 7);
        assert_eq!(
            tag(postcard::to_stdvec(&ShimMsg::Version { build_id: String::new(), handoff: 0, generation: 0 }).unwrap()),
            8
        );
    }
}
