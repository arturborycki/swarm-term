//! Program status: what the programs in a session say about themselves.
//!
//! The main source is the Program Status Protocol, OSC 7501 (described in
//! the Swarm docs, under "Program status"): a program
//! reports `idle`, `working`, `done`, `blocked` or `error`, with an optional
//! reason, progress and app name, for itself (the root record) or for named
//! pieces of work (`build/test`). Each report replaces its record whole.
//! `working` and `blocked` records end when the shell shows its next prompt
//! or the program exits; `done` and `error` stay until the user has seen the
//! session.
//!
//! Until a program sends OSC 7501, OSC 9;4 progress (ConEmu, Windows
//! Terminal, Ghostty) is mapped onto the root record. Bells, desktop
//! notifications (OSC 9, 777, 99) and command exit codes (OSC 133;D) are kept
//! beside the records.
//!
//! Everything here comes from program output, so it is untrusted: limits are
//! checked before anything is stored, and text with control characters is
//! refused.

use swarm_proto::{BlockedKind, Notification, ProgramStatus, StatusRecord, StatusState};

/// What the scanner saw that matters here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusSignal {
    /// The body of `OSC 7501 ; <body> ST`.
    Report(Vec<u8>),
    /// `OSC 7501 ; ? ST`: the program asks whether the protocol is supported.
    Query,
    /// `OSC 9 ; 4 ; <state> ; <progress> ST`.
    Progress { state: u8, value: Option<u8> },
    Notify { title: String, body: String },
    Bell,
    /// `OSC 133 ; D [; <exit code>] ST`.
    CommandEnd(Option<i32>),
    /// A new shell prompt (`OSC 133 ; A`, or Warp's precmd hook).
    Prompt,
    /// Full reset (RIS, `ESC c`).
    Reset,
}

/// The reply to `OSC 7501 ; ?`.
pub const QUERY_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

/// Records kept per session. The protocol asks for at least 64.
const MAX_RECORDS: usize = 64;
/// Longest report body: the whole sequence may be 4096 bytes.
const MAX_BODY: usize = 4096 - 8;
const MAX_KEY: usize = 16;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG: usize = 2048;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE: usize = 192;
const MAX_APP: usize = 32;
const MAX_ID: usize = 128;
const MAX_ID_SEGMENT: usize = 32;
const MAX_ID_DEPTH: usize = 8;
/// Longest notification title or body kept.
const MAX_NOTIFICATION: usize = 1024;

#[derive(Default)]
pub struct StatusBook {
    status: ProgramStatus,
    /// An OSC 7501 report arrived since the last full reset: OSC 9;4 no
    /// longer maps onto the root record (it would wipe its kind and message).
    saw_report: bool,
}

/// A valid OSC 7501 report.
#[derive(Debug, PartialEq, Eq)]
struct Report {
    id: String,
    /// `None` is `clear`.
    state: Option<StatusState>,
    kind: Option<BlockedKind>,
    progress: Option<u8>,
    app: Option<String>,
    title: Option<String>,
    msg: Option<String>,
}

impl StatusBook {
    pub fn snapshot(&self) -> ProgramStatus {
        self.status.clone()
    }

    /// Applies one signal; returns whether anything changed.
    pub fn apply(&mut self, signal: StatusSignal, now: u64) -> bool {
        let changed = match signal {
            StatusSignal::Report(body) => match parse_report(&body) {
                Some(report) => {
                    self.saw_report = true;
                    self.apply_report(report, now)
                }
                None => false,
            },
            StatusSignal::Query => false,
            StatusSignal::Progress { state, value } => !self.saw_report && self.apply_progress(state, value, now),
            StatusSignal::Notify { title, body } => {
                let (title, body) = (clean(&title), clean(&body));
                if title.is_empty() && body.is_empty() {
                    false
                } else {
                    self.status.notification = Some(Notification { title, body, at_ms: now });
                    true
                }
            }
            StatusSignal::Bell => {
                self.status.bells += 1;
                self.status.bell_ms = now;
                true
            }
            StatusSignal::CommandEnd(code) => match code {
                Some(code) => {
                    self.status.last_exit = Some((code, now));
                    true
                }
                None => false,
            },
            StatusSignal::Prompt => self.end_live_records(),
            StatusSignal::Reset => {
                self.saw_report = false;
                let had = !self.status.records.is_empty();
                self.status.records.clear();
                had
            }
        };
        if changed {
            self.status.changed_ms = now;
        }
        changed
    }

    /// The program exited: its `working` and `blocked` records end.
    pub fn on_exit(&mut self, now: u64) -> bool {
        let changed = self.end_live_records();
        if changed {
            self.status.changed_ms = now;
        }
        changed
    }

    /// The user has seen the session: `done` and `error` records and the
    /// last notification go.
    pub fn clear_seen(&mut self, now: u64) -> bool {
        let before = self.status.records.len();
        self.status.records.retain(|r| !matches!(r.state, StatusState::Done | StatusState::Error));
        let notified = self.status.notification.take().is_some();
        let changed = self.status.records.len() != before || notified;
        if changed {
            self.status.changed_ms = now;
        }
        changed
    }

    fn end_live_records(&mut self) -> bool {
        let before = self.status.records.len();
        self.status.records.retain(|r| !matches!(r.state, StatusState::Working | StatusState::Blocked));
        self.status.records.len() != before
    }

    fn apply_report(&mut self, r: Report, now: u64) -> bool {
        let records = &mut self.status.records;
        let Some(state) = r.state else {
            // `clear`: the record and everything beneath it.
            let before = records.len();
            records.retain(|rec| !is_within(&rec.id, &r.id));
            return records.len() != before;
        };
        let record = StatusRecord {
            id: r.id,
            state,
            kind: r.kind.filter(|_| state == StatusState::Blocked),
            progress: r.progress.filter(|_| matches!(state, StatusState::Working | StatusState::Blocked)),
            app: r.app,
            title: r.title,
            msg: r.msg,
            updated_ms: now,
        };
        match records.binary_search_by(|rec| rec.id.cmp(&record.id)) {
            Ok(i) => records[i] = record,
            Err(i) => {
                records.insert(i, record);
                if records.len() > MAX_RECORDS {
                    // The record updated least recently makes room.
                    let oldest = records.iter().enumerate().min_by_key(|(_, rec)| rec.updated_ms).map(|(i, _)| i);
                    if let Some(oldest) = oldest {
                        records.remove(oldest);
                    }
                }
            }
        }
        true
    }

    /// OSC 9;4: 0 remove, 1 set, 2 error, 3 indeterminate, 4 paused.
    fn apply_progress(&mut self, state: u8, value: Option<u8>, now: u64) -> bool {
        let report = match state {
            0 => Report { id: String::new(), state: None, kind: None, progress: None, app: None, title: None, msg: None },
            1 | 4 => Report {
                id: String::new(),
                state: Some(StatusState::Working),
                kind: None,
                progress: value.filter(|v| *v <= 100),
                app: None,
                title: None,
                msg: None,
            },
            2 => Report { id: String::new(), state: Some(StatusState::Error), kind: None, progress: None, app: None, title: None, msg: None },
            3 => Report { id: String::new(), state: Some(StatusState::Working), kind: None, progress: None, app: None, title: None, msg: None },
            _ => return false,
        };
        // A repeated progress report with the same values changes nothing.
        if let Some(cur) = self.status.records.iter().find(|r| r.id.is_empty()) {
            if Some(cur.state) == report.state && cur.progress == report.progress && cur.msg.is_none() {
                return false;
            }
        }
        self.apply_report(report, now)
    }
}

/// Whether `id` is `parent` or beneath it (`""` is above everything).
fn is_within(id: &str, parent: &str) -> bool {
    parent.is_empty() || id == parent || id.strip_prefix(parent).is_some_and(|rest| rest.starts_with('/'))
}

/// Text from a notification: control characters dropped, length capped.
fn clean(text: &str) -> String {
    let mut out: String = text.chars().filter(|c| !c.is_control()).collect();
    if out.len() > MAX_NOTIFICATION {
        let mut end = MAX_NOTIFICATION;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

/// Parses an OSC 7501 body. `None`: the report is discarded whole.
fn parse_report(body: &[u8]) -> Option<Report> {
    if body.len() > MAX_BODY {
        return None;
    }
    let body = std::str::from_utf8(body).ok()?;
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for pair in body.split(':') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        if key.len() > MAX_KEY {
            return None;
        }
        let key_ok = !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase());
        let value_ok = value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b));
        if !key_ok || !value_ok {
            // A malformed pair is skipped; the rest still counts.
            continue;
        }
        // The last value of a repeated key wins.
        pairs.retain(|(k, _)| *k != key);
        pairs.push((key, value));
    }
    let get = |k: &str| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);

    let state = match get("state")? {
        "idle" => Some(StatusState::Idle),
        "working" => Some(StatusState::Working),
        "done" => Some(StatusState::Done),
        "blocked" => Some(StatusState::Blocked),
        "error" => Some(StatusState::Error),
        "clear" => None,
        // A state from a later revision must not turn into something else.
        _ => return None,
    };
    let id = match get("id") {
        None => String::new(),
        Some(id) => {
            let segments: Vec<&str> = id.split('/').collect();
            let valid = id.len() <= MAX_ID
                && segments.len() <= MAX_ID_DEPTH
                && segments.iter().all(|s| {
                    !s.is_empty() && s.len() <= MAX_ID_SEGMENT && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
                });
            if !valid {
                return None;
            }
            id.to_string()
        }
    };
    let kind = match get("kind") {
        Some("permission") => Some(BlockedKind::Permission),
        Some("question") => Some(BlockedKind::Question),
        Some("auth") => Some(BlockedKind::Auth),
        _ => None,
    };
    let progress = get("progress")
        .filter(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|p| p.parse::<u8>().ok())
        .filter(|p| *p <= 100);
    let app = get("app")
        .filter(|a| a.len() <= MAX_APP && !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b)))
        .map(str::to_string);
    let title = match get("title") {
        Some(t) => Some(decode_text(t, MAX_TITLE_ENCODED, MAX_TITLE)?),
        None => None,
    };
    let msg = match get("msg") {
        Some(m) => Some(decode_text(m, MAX_MSG_ENCODED, MAX_MSG)?),
        None => None,
    };
    Some(Report { id, state, kind, progress, app, title, msg })
}

/// Base64 UTF-8 text with no control characters, within the limits.
fn decode_text(encoded: &str, max_encoded: usize, max_decoded: usize) -> Option<String> {
    if encoded.len() > max_encoded {
        return None;
    }
    let bytes = base64_decode(encoded)?;
    if bytes.len() > max_decoded {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(text)
}

/// Standard base64, padding optional.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be padding zeros, and a lone 6-bit group is invalid.
    if bits >= 6 || acc != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in s.as_bytes().chunks(3) {
            let n = chunk.iter().fold(0u32, |a, b| (a << 8) | *b as u32) << (8 * (3 - chunk.len()));
            for i in 0..=chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        out
    }

    fn report(book: &mut StatusBook, body: &str, now: u64) -> bool {
        book.apply(StatusSignal::Report(body.as_bytes().to_vec()), now)
    }

    #[test]
    fn base64_round_trip_and_rejects() {
        for s in ["", "a", "ab", "abc", "Apply 3 to add?", "naïve ✓"] {
            assert_eq!(base64_decode(&b64(s)).unwrap(), s.as_bytes(), "{s}");
        }
        assert_eq!(base64_decode("SGk=").unwrap(), b"Hi");
        assert!(base64_decode("S").is_none());
        assert!(base64_decode("SG!k").is_none());
    }

    #[test]
    fn the_spec_example_is_read_whole() {
        let mut book = StatusBook::default();
        let body = "state=blocked:kind=permission:app=terraform:msg=QXBwbHkgMyB0byBhZGQsIDEgdG8gY2hhbmdlLCAwIHRvIGRlc3Ryb3k/";
        assert!(report(&mut book, body, 5));
        let s = book.snapshot();
        let r = &s.records[0];
        assert_eq!(r.id, "");
        assert_eq!(r.state, StatusState::Blocked);
        assert_eq!(r.kind, Some(BlockedKind::Permission));
        assert_eq!(r.app.as_deref(), Some("terraform"));
        assert_eq!(r.msg.as_deref(), Some("Apply 3 to add, 1 to change, 0 to destroy?"));
        assert_eq!(s.changed_ms, 5);
    }

    #[test]
    fn each_report_replaces_its_record_whole() {
        let mut book = StatusBook::default();
        report(&mut book, &format!("state=working:app=brew:msg={}", b64("Installing")), 1);
        report(&mut book, "state=done", 2);
        let r = &book.snapshot().records[0];
        assert_eq!(r.state, StatusState::Done);
        assert_eq!(r.app, None, "a key left out is gone afterwards");
    }

    #[test]
    fn bad_reports_are_dropped_whole_and_bad_pairs_skipped() {
        let mut book = StatusBook::default();
        assert!(!report(&mut book, "msg=SGk=", 1), "no state");
        assert!(!report(&mut book, "state=sleeping", 1), "unknown state");
        assert!(!report(&mut book, "state=working:id=a//b", 1), "bad id");
        assert!(!report(&mut book, &format!("state=working:msg={}", b64("two\nlines")), 1), "control character");
        // A byte outside the value set drops that pair only.
        assert!(report(&mut book, "state=working:msg=@@@", 1));
        assert!(!report(&mut book, "state=working:msg=S", 1), "bad base64");
        assert!(!report(&mut book, "state=working:averyveryverylongkey=1", 1), "key too long");
        assert!(book.snapshot().records.iter().all(|r| r.msg.is_none()));
        // A malformed pair is skipped, the rest applies; unknown keys are ignored.
        assert!(report(&mut book, "state=working:nonsense:future=x:progress=40", 2));
        assert!(report(&mut book, "state=working:progress=+40", 2));
        assert_eq!(book.snapshot().records[0].progress, None);
        report(&mut book, "state=working:nonsense:future=x:progress=40", 2);
        let r = &book.snapshot().records[0];
        assert_eq!((r.state, r.progress), (StatusState::Working, Some(40)));
        // kind only with blocked, progress only with working or blocked.
        report(&mut book, "state=done:kind=auth:progress=10", 3);
        let r = &book.snapshot().records[0];
        assert_eq!((r.kind, r.progress), (None, None));
        report(&mut book, "state=working:progress=101", 4);
        assert_eq!(book.snapshot().records[0].progress, None);
    }

    #[test]
    fn records_nest_clear_and_inherit_their_app() {
        let mut book = StatusBook::default();
        report(&mut book, "state=working:app=deploy", 1);
        report(&mut book, "state=working:id=us-east:progress=40", 2);
        report(&mut book, &format!("state=blocked:kind=permission:id=eu-west:msg={}", b64("Approve?")), 3);
        report(&mut book, "state=working:id=eu-west/db", 4);
        let s = book.snapshot();
        assert_eq!(s.records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["", "eu-west", "eu-west/db", "us-east"]);
        assert_eq!(s.app_of(&s.records[2]), Some("deploy"));
        let head = s.headline().unwrap();
        assert_eq!((head.id.as_str(), head.state), ("eu-west", StatusState::Blocked));
        report(&mut book, "state=clear:id=eu-west", 5);
        assert_eq!(book.snapshot().records.len(), 2);
        report(&mut book, "state=clear", 6);
        assert!(book.snapshot().records.is_empty());
    }

    #[test]
    fn prompts_and_exits_end_live_records_but_not_results() {
        let mut book = StatusBook::default();
        report(&mut book, "state=working:id=a", 1);
        report(&mut book, "state=blocked:id=b", 1);
        report(&mut book, "state=done:id=c", 1);
        report(&mut book, "state=idle:id=d", 1);
        assert!(book.apply(StatusSignal::Prompt, 2));
        let ids: Vec<String> = book.snapshot().records.into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["c", "d"]);
        report(&mut book, "state=error:id=e", 3);
        assert!(book.clear_seen(4));
        assert_eq!(book.snapshot().records.len(), 1);
        report(&mut book, "state=working", 5);
        assert!(book.on_exit(6));
        assert!(!book.on_exit(7));
    }

    #[test]
    fn osc_9_4_maps_to_the_root_until_a_real_report() {
        let mut book = StatusBook::default();
        assert!(book.apply(StatusSignal::Progress { state: 1, value: Some(30) }, 1));
        assert!(!book.apply(StatusSignal::Progress { state: 1, value: Some(30) }, 2), "same values");
        assert_eq!(book.snapshot().records[0].progress, Some(30));
        book.apply(StatusSignal::Progress { state: 2, value: None }, 3);
        assert_eq!(book.snapshot().records[0].state, StatusState::Error);
        book.apply(StatusSignal::Progress { state: 0, value: None }, 4);
        assert!(book.snapshot().records.is_empty());
        report(&mut book, &format!("state=working:msg={}", b64("Deploying")), 5);
        assert!(!book.apply(StatusSignal::Progress { state: 0, value: None }, 6), "ignored after OSC 7501");
        assert!(book.apply(StatusSignal::Reset, 7));
        assert!(book.apply(StatusSignal::Progress { state: 3, value: None }, 8), "mapped again after a reset");
    }

    #[test]
    fn the_least_recently_updated_record_makes_room() {
        let mut book = StatusBook::default();
        for i in 0..MAX_RECORDS as u64 + 1 {
            report(&mut book, &format!("state=working:id=r{i}"), 10 + i);
        }
        let s = book.snapshot();
        assert_eq!(s.records.len(), MAX_RECORDS);
        assert!(s.records.iter().all(|r| r.id != "r0"));
    }

    #[test]
    fn bells_notifications_and_exit_codes() {
        let mut book = StatusBook::default();
        book.apply(StatusSignal::Bell, 1);
        book.apply(StatusSignal::Bell, 2);
        book.apply(StatusSignal::Notify { title: "Claude".into(), body: "Needs\x07 input".into() }, 3);
        book.apply(StatusSignal::CommandEnd(Some(2)), 4);
        report(&mut book, "state=done", 4);
        let s = book.snapshot();
        assert_eq!((s.bells, s.bell_ms), (2, 2));
        assert_eq!(s.notification.as_ref().map(|n| n.body.as_str()), Some("Needs input"));
        assert_eq!(s.last_exit, Some((2, 4)));
        assert!(book.clear_seen(5));
        let s = book.snapshot();
        assert!(s.notification.is_none() && s.records.is_empty(), "both go in one call");
    }
}
