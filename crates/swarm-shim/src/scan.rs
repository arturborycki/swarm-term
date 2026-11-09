//! Lightweight scan of PTY output for the few things the shim needs to know:
//! title, cwd, alt-screen state, safe replay boundaries and Warp's shell
//! hooks. No grid is kept.

use vte::{Params, Parser, Perform};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanEvents {
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// `Some(true)` on entering the alternate screen, `Some(false)` on leaving.
    pub alt_screen: Option<bool>,
    /// Absolute offsets where replay may start.
    pub boundaries: Vec<u64>,
    /// Warp lifecycle hooks (`ESC P $ d <hex JSON> ESC \\`) that ended here.
    pub hooks: Vec<WarpHook>,
}

/// A Warp shell hook: its name (`InitShell`, `Bootstrapped`, `Precmd`, …) and
/// where it starts and ends in the output (`end` is just past the ESC of its
/// terminator; the final `\\` may arrive in the next chunk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarpHook {
    pub name: String,
    pub start: u64,
    pub end: u64,
}

/// Longest OSC sequence the parser keeps (titles, folder URLs). Longer ones
/// are cut off by the parser.
const OSC_BUF: usize = 4096;
/// Longest title or folder kept: session info is sent whole in one frame,
/// so output must not be able to make it big.
pub const MAX_TEXT: usize = 2048;

/// `text` cut to at most `MAX_TEXT` bytes, at a character boundary.
fn capped(mut text: String) -> String {
    if text.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Enough of the hex payload to read `{"hook": "<name>"`.
const HOOK_NAME_HEX_PREFIX: usize = 160;

pub struct Scanner {
    parser: Parser<OSC_BUF>,
    perf: Collector,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner { parser: Parser::new_with_size(), perf: Collector::default() }
    }
}

#[derive(Default)]
struct Collector {
    /// Offset of the byte being fed, and the byte itself.
    pos: u64,
    byte: u8,
    /// Offset of the ESC that began the current sequence.
    seq_start: u64,
    /// `seq_start` before the latest ESC: when an `ESC \\` terminator
    /// triggers a dispatch, the sequence began here.
    prev_seq_start: u64,
    /// A Warp DCS hook being received: its start and the hex payload's prefix.
    dcs: Option<(u64, Vec<u8>)>,
    events: ScanEvents,
}

impl Collector {
    /// Start of the sequence being dispatched right now.
    fn dispatch_start(&self) -> u64 {
        if self.byte == 0x1b {
            self.prev_seq_start
        } else {
            self.seq_start
        }
    }
}

impl Scanner {
    /// Feeds bytes that start at absolute offset `offset`.
    pub fn feed(&mut self, offset: u64, bytes: &[u8]) -> ScanEvents {
        for (i, b) in bytes.iter().enumerate() {
            let pos = offset + i as u64;
            self.perf.pos = pos;
            self.perf.byte = *b;
            if *b == 0x1b || *b == 0x9b || *b == 0x9d || *b == 0x90 {
                self.perf.prev_seq_start = self.perf.seq_start;
                self.perf.seq_start = pos;
            }
            self.parser.advance(&mut self.perf, std::slice::from_ref(b));
        }
        std::mem::take(&mut self.perf.events)
    }
}

impl Perform for Collector {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(kind) = params.first() else { return };
        match *kind {
            // vte splits on ';', which titles and URLs may contain: rejoin.
            b"0" | b"2" if params.len() > 1 => {
                let joined = params[1..].join(&b';');
                self.events.title = Some(capped(String::from_utf8_lossy(&joined).into_owned()));
            }
            b"7" if params.len() > 1 => {
                let url = String::from_utf8_lossy(&params[1..].join(&b';')).into_owned();
                if let Some(cwd) = cwd_from_file_url(&url).filter(|c| c.len() <= MAX_TEXT) {
                    self.events.cwd = Some(cwd);
                }
            }
            // FinalTerm / iTerm2 / VS Code shell integration: prompt start.
            b"133" if params.get(1).is_some_and(|p| p.first() == Some(&b'A')) => {
                self.events.boundaries.push(self.dispatch_start());
            }
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        if intermediates == b"$" && action == 'd' {
            self.dcs = Some((self.seq_start, Vec::new()));
        }
    }

    fn put(&mut self, byte: u8) {
        if let Some((_, hex)) = &mut self.dcs {
            if hex.len() < HOOK_NAME_HEX_PREFIX {
                hex.push(byte);
            }
        }
    }

    fn unhook(&mut self) {
        let Some((start, hex)) = self.dcs.take() else { return };
        if let Some(name) = hook_name(&hex) {
            self.events.hooks.push(WarpHook { name, start, end: self.pos + 1 });
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        if intermediates != b"?" || (action != 'h' && action != 'l') {
            return;
        }
        let alt = params.iter().any(|p| matches!(p.first(), Some(47 | 1047 | 1049)));
        if !alt {
            return;
        }
        if action == 'h' {
            self.events.alt_screen = Some(true);
        } else {
            self.events.alt_screen = Some(false);
            // Back on the primary screen: the next byte is a clean start.
            self.events.boundaries.push(self.pos + 1);
        }
    }
}

/// Reads the hook name from the start of a hex-encoded `{"hook": "<name>", …}`.
fn hook_name(hex: &[u8]) -> Option<String> {
    let even = &hex[..hex.len() & !1];
    let bytes: Vec<u8> = even
        .chunks(2)
        .map_while(|p| std::str::from_utf8(p).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect();
    let text = String::from_utf8_lossy(&bytes);
    let rest = &text[text.find("\"hook\"")? + 6..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (!name.is_empty()).then_some(name)
}

/// `file://host/path%20with%20spaces` -> `/path with spaces`.
fn cwd_from_file_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let bytes = &rest.as_bytes()[rest.find('/')?..];
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_and_cwd() {
        let mut s = Scanner::default();
        let ev = s.feed(0, b"\x1b]2;vim; main.rs\x07\x1b]7;file://mac.local/Users/me/my%20dir\x1b\\");
        assert_eq!(ev.title.as_deref(), Some("vim; main.rs"));
        assert_eq!(ev.cwd.as_deref(), Some("/Users/me/my dir"));
    }

    #[test]
    fn huge_titles_are_cut_and_buffered_only_up_to_a_limit() {
        let mut s = Scanner::default();
        let mut out = b"\x1b]2;".to_vec();
        out.extend(std::iter::repeat_n(b'x', 20_000_000));
        out.push(0x07);
        let ev = s.feed(0, &out);
        let title = ev.title.unwrap_or_default();
        assert!(title.len() <= MAX_TEXT, "{}", title.len());
        // And it keeps working afterwards.
        let ev = s.feed(out.len() as u64, b"\x1b]2;next\x07");
        assert_eq!(ev.title.as_deref(), Some("next"));
    }

    #[test]
    fn prompt_boundary_points_at_sequence_start() {
        let mut s = Scanner::default();
        let ev = s.feed(100, b"out\r\n\x1b]133;A\x07$ ");
        assert_eq!(ev.boundaries, vec![105]);
    }

    #[test]
    fn st_terminated_prompt_marker_points_at_its_own_start() {
        let mut s = Scanner::default();
        let ev = s.feed(10, b"ab\x1b]133;A\x1b\\$ ");
        assert_eq!(ev.boundaries, vec![12]);
    }

    fn warp_hook(json: &str) -> Vec<u8> {
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        let mut v = b"\x1bP$d".to_vec();
        v.extend(hex.into_bytes());
        v.extend(b"\x1b\\");
        v
    }

    #[test]
    fn warp_hooks_are_named_and_located() {
        let mut s = Scanner::default();
        let mut out = b"xy".to_vec();
        let precmd = warp_hook(r#"{"hook": "Precmd", "value": {"session_id": 7}}"#);
        out.extend(&precmd);
        // Split mid-payload to cover chunk boundaries.
        let (a, b) = out.split_at(20);
        let mut hooks = s.feed(100, a).hooks;
        hooks.extend(s.feed(100 + a.len() as u64, b).hooks);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].name, "Precmd");
        assert_eq!(hooks[0].start, 102);
        // end is just past the terminator's ESC.
        assert_eq!(hooks[0].end, 102 + precmd.len() as u64 - 1);
    }

    #[test]
    fn other_dcs_sequences_are_ignored() {
        let mut s = Scanner::default();
        let ev = s.feed(0, b"\x1bP1$r0m\x1b\\");
        assert!(ev.hooks.is_empty());
    }

    #[test]
    fn alt_screen_enter_and_exit_across_chunks() {
        let mut s = Scanner::default();
        let ev = s.feed(0, b"ab\x1b[?10");
        assert_eq!(ev.alt_screen, None);
        let ev = s.feed(7, b"49hvim");
        assert_eq!(ev.alt_screen, Some(true));
        let ev = s.feed(13, b"\x1b[?1049l$");
        assert_eq!(ev.alt_screen, Some(false));
        // Boundary right after the final 'l' (the sequence spans 13..21).
        assert_eq!(ev.boundaries, vec![21]);
    }
}
