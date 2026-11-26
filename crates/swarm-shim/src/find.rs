//! Search in a session's output (`swarmctl grep`, Find in All Sessions): the
//! output is turned back into lines of text, escape sequences removed, and
//! lines with the text are kept, the most recent ones.

use std::collections::VecDeque;

use swarm_proto::{SearchHit, SearchOpts};
use vte::{Params, Parser, Perform};

/// Longest line kept; the rest of a longer one is ignored.
const MAX_LINE: usize = 4096;

/// Characters of a matching line sent around the match.
const SHOWN: usize = 160;

/// How much output, at most, is searched per session: the most recent.
pub const MAX_SEARCHED: u64 = 64 * 1024 * 1024;

pub struct Finder {
    parser: Parser,
    lines: Lines,
}

struct Lines {
    needle: Vec<char>,
    case_sensitive: bool,
    max_hits: usize,
    pos: u64,
    /// The line being read and the offset it started at.
    line: String,
    start: u64,
    /// A carriage return was read: the next character starts the line over,
    /// as it would on a screen (progress bars redraw their line this way).
    cr: bool,
    hits: VecDeque<SearchHit>,
    total: u64,
}

impl Finder {
    pub fn new(opts: &SearchOpts, from: u64) -> Finder {
        let case_sensitive = opts.is_case_sensitive();
        let fold = |c: char| if case_sensitive { c } else { c.to_lowercase().next().unwrap_or(c) };
        Finder {
            parser: Parser::new(),
            lines: Lines {
                needle: opts.text.chars().map(fold).collect(),
                case_sensitive,
                max_hits: opts.max_hits.max(1) as usize,
                pos: from,
                line: String::new(),
                start: from,
                cr: false,
                hits: VecDeque::new(),
                total: 0,
            },
        }
    }

    /// Feeds the output that starts at `offset`. Pieces come in order; a gap
    /// (output dropped since) starts a new line.
    pub fn feed(&mut self, offset: u64, bytes: &[u8]) {
        if offset != self.lines.pos {
            self.lines.end_line();
            self.parser = Parser::new();
            self.lines.pos = offset;
            self.lines.start = offset;
        }
        // The parser reports no positions: feed it in runs that end at a
        // newline, so a line's start is known.
        let mut rest = bytes;
        while !rest.is_empty() {
            let cut = rest.iter().position(|&b| b == b'\n').map_or(rest.len(), |i| i + 1);
            let (run, tail) = rest.split_at(cut);
            self.parser.advance(&mut self.lines, run);
            self.lines.pos += run.len() as u64;
            if run.ends_with(b"\n") {
                self.lines.start = self.lines.pos;
            }
            rest = tail;
        }
    }

    /// The matching lines, newest first, and how many matched in all.
    pub fn finish(mut self) -> (Vec<SearchHit>, u64) {
        self.lines.end_line();
        (self.lines.hits.into_iter().rev().collect(), self.lines.total)
    }
}

impl Lines {
    fn end_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        self.cr = false;
        if self.needle.is_empty() {
            return;
        }
        let Some(at) = self.find(&line) else { return };
        self.total += 1;
        if self.hits.len() == self.max_hits {
            self.hits.pop_front();
        }
        self.hits.push_back(shown(&line, at, self.needle.len(), self.start));
    }

    fn find(&self, line: &str) -> Option<usize> {
        let chars: Vec<char> = if self.case_sensitive {
            line.chars().collect()
        } else {
            line.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
        };
        chars.windows(self.needle.len()).position(|w| w == self.needle.as_slice())
    }
}

/// The part of `line` around the match at character `at`.
fn shown(line: &str, at: usize, len: usize, offset: u64) -> SearchHit {
    let chars: Vec<char> = line.chars().collect();
    let room = SHOWN.saturating_sub(len);
    let mut from = at.saturating_sub(room / 3);
    let to = (from + SHOWN).min(chars.len()).max(at + len);
    if to - from < SHOWN {
        from = to.saturating_sub(SHOWN);
    }
    let mut text: String = chars[from..to].iter().collect();
    let mut start = at - from;
    if from > 0 {
        text.insert(0, '…');
        start += 1;
    }
    if to < chars.len() {
        text.push('…');
    }
    // Leading blanks only push the match to the right.
    let trimmed = text.trim_start();
    let cut = text.chars().count() - trimmed.chars().count();
    let (line, start) = if cut <= start { (trimmed.trim_end().to_string(), start - cut) } else { (text.trim_end().to_string(), start) };
    SearchHit { offset, line, at: (start as u32, len as u32) }
}

impl Perform for Lines {
    fn print(&mut self, c: char) {
        if self.cr {
            self.line.clear();
            self.cr = false;
        }
        if self.line.len() < MAX_LINE {
            self.line.push(c);
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.end_line(),
            b'\r' => self.cr = true,
            b'\t' => self.print('\t'),
            // Backspace: the shell's line editor moves back over what it drew.
            0x08 => {
                self.line.pop();
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        // Cursor forward: some programs move over blanks instead of printing them.
        if action == 'C' {
            let n = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(1).clamp(1, 256);
            for _ in 0..n {
                self.print(' ');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(text: &str, output: &[&[u8]]) -> (Vec<SearchHit>, u64) {
        let opts = SearchOpts { text: text.into(), case_sensitive: None, max_hits: 3 };
        let mut f = Finder::new(&opts, 0);
        let mut at = 0;
        for piece in output {
            f.feed(at, piece);
            at += piece.len() as u64;
        }
        f.finish()
    }

    fn lines(hits: &[SearchHit]) -> Vec<&str> {
        hits.iter().map(|h| h.line.as_str()).collect()
    }

    #[test]
    fn finds_lines_newest_first() {
        let (hits, total) = search("error", &[b"ok\nerror: one\nfine\r\nAn Error two\n"]);
        assert_eq!(lines(&hits), ["An Error two", "error: one"]);
        assert_eq!(total, 2);
        assert_eq!(hits[1].offset, 3);
        assert_eq!(hits[0].at, (3, 5));
    }

    #[test]
    fn capitals_make_it_case_sensitive() {
        let (hits, _) = search("Error", &[b"error\nError\n"]);
        assert_eq!(lines(&hits), ["Error"]);
    }

    #[test]
    fn removes_escape_sequences() {
        let (hits, _) = search("build ok", &[b"\x1b[1;32mbuild\x1b[0m ok\x1b]0;title\x07\n"]);
        assert_eq!(lines(&hits), ["build ok"]);
        assert_eq!(hits[0].at, (0, 8));
    }

    #[test]
    fn carriage_return_starts_the_line_over() {
        let (hits, _) = search("100%", &[b"10%\r50%\r100% done\n"]);
        assert_eq!(lines(&hits), ["100% done"]);
        let (hits, _) = search("10%", &[b"10%\r50%\r100% done\n"]);
        assert!(hits.is_empty());
    }

    #[test]
    fn lines_split_across_pieces() {
        let (hits, _) = search("needle", &[b"hay nee", b"dle hay\n"]);
        assert_eq!(lines(&hits), ["hay needle hay"]);
        assert_eq!(hits[0].offset, 0);
    }

    #[test]
    fn keeps_the_most_recent() {
        let (hits, total) = search("x", &[b"x1\nx2\nx3\nx4\nx5"]);
        assert_eq!(lines(&hits), ["x5", "x4", "x3"]);
        assert_eq!(total, 5);
    }

    #[test]
    fn a_gap_ends_the_line() {
        let opts = SearchOpts { text: "ab".into(), case_sensitive: None, max_hits: 5 };
        let mut f = Finder::new(&opts, 0);
        f.feed(0, b"a");
        f.feed(100, b"b\n");
        assert!(f.finish().0.is_empty());
    }

    #[test]
    fn long_lines_are_shortened_around_the_match() {
        let line = format!("{}needle{}\n", "a".repeat(500), "b".repeat(500));
        let (hits, _) = search("needle", &[line.as_bytes()]);
        let hit = &hits[0];
        assert!(hit.line.starts_with('…') && hit.line.ends_with('…'));
        let shown: String = hit.line.chars().skip(hit.at.0 as usize).take(hit.at.1 as usize).collect();
        assert_eq!(shown, "needle");
        assert!(hit.line.chars().count() <= SHOWN + 2);
    }

    #[test]
    fn leading_blanks_are_trimmed() {
        let (hits, _) = search("x", &[b"    x\n"]);
        assert_eq!(hits[0].line, "x");
        assert_eq!(hits[0].at, (0, 1));
    }
}
