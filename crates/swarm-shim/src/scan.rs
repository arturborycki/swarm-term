//! Lightweight scan of PTY output for the few things the shim needs to know:
//! title, cwd, alt-screen state and safe replay boundaries. No grid is kept.

use vte::{Params, Parser, Perform};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanEvents {
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// `Some(true)` on entering the alternate screen, `Some(false)` on leaving.
    pub alt_screen: Option<bool>,
    /// Absolute offsets where replay may start.
    pub boundaries: Vec<u64>,
}

pub struct Scanner {
    parser: Parser,
    perf: Collector,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner { parser: Parser::new(), perf: Collector::default() }
    }
}

#[derive(Default)]
struct Collector {
    /// Offset of the byte being fed.
    pos: u64,
    /// Offset of the ESC that began the current sequence.
    seq_start: u64,
    events: ScanEvents,
}

impl Scanner {
    /// Feeds bytes that start at absolute offset `offset`.
    pub fn feed(&mut self, offset: u64, bytes: &[u8]) -> ScanEvents {
        for (i, b) in bytes.iter().enumerate() {
            let pos = offset + i as u64;
            self.perf.pos = pos;
            if *b == 0x1b || *b == 0x9b || *b == 0x9d {
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
                self.events.title = Some(String::from_utf8_lossy(&joined).into_owned());
            }
            b"7" if params.len() > 1 => {
                let url = String::from_utf8_lossy(&params[1..].join(&b';')).into_owned();
                if let Some(cwd) = cwd_from_file_url(&url) {
                    self.events.cwd = Some(cwd);
                }
            }
            // FinalTerm / iTerm2 / VS Code shell integration: prompt start.
            b"133" if params.get(1).is_some_and(|p| p.first() == Some(&b'A')) => {
                self.events.boundaries.push(self.seq_start);
            }
            _ => {}
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
    fn prompt_boundary_points_at_sequence_start() {
        let mut s = Scanner::default();
        let ev = s.feed(100, b"out\r\n\x1b]133;A\x07$ ");
        assert_eq!(ev.boundaries, vec![105]);
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
