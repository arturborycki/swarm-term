//! The session's screen model (`vt100`), kept so a full-screen app's screen
//! can be redrawn on attach.
//!
//! `vt100` panics on some ordinary output, more so after resizes (0.15.2
//! fails at `screen.rs:867` and `:934`). The model is only used for that
//! redraw, so a panic must never take the session with it: every call is
//! made inside `catch_unwind`, and a model that panicked is replaced by an
//! empty one of the same size. The next full-screen redraw by the app fills
//! it again.

use std::panic::{catch_unwind, AssertUnwindSafe};

pub struct Screen {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Screen {
        let (rows, cols) = (rows.max(1), cols.max(1));
        Screen { parser: vt100::Parser::new(rows, cols, 0), rows, cols }
    }

    pub fn process(&mut self, data: &[u8]) {
        self.guard("process output", |p| p.process(data));
    }

    pub fn set_size(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        self.rows = rows;
        self.cols = cols;
        self.guard("resize", |p| p.set_size(rows, cols));
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    pub fn alternate(&mut self) -> bool {
        self.guard("read the screen", |p| p.screen().alternate_screen()).unwrap_or(false)
    }

    /// Escape sequences that redraw the screen (empty if the model failed).
    pub fn formatted(&mut self) -> Vec<u8> {
        self.guard("draw the screen", |p| p.screen().state_formatted()).unwrap_or_default()
    }

    /// Runs `f` on the parser; if it panics, starts over with an empty model.
    fn guard<T>(&mut self, what: &str, f: impl FnOnce(&mut vt100::Parser) -> T) -> Option<T> {
        match catch_unwind(AssertUnwindSafe(|| f(&mut self.parser))) {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!("the screen model failed to {what}; starting it over (the session is unaffected)");
                self.parser = vt100::Parser::new(self.rows, self.cols, 0);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_model_is_replaced_and_keeps_working() {
        // A wide character in a one-column terminal panics vt100 0.15.2.
        let mut s = Screen::new(24, 1);
        s.process("漢".as_bytes());
        s.set_size(24, 80);
        s.process(b"\x1b[?1049hhello");
        assert!(s.alternate());
        assert!(!s.formatted().is_empty());
    }

    #[test]
    fn resizes_mixed_with_output_never_escape() {
        // Sequences found by fuzzing that panic inside vt100 0.15.2.
        let mut s = Screen::new(1, 5);
        s.process("aaaa漢漢".as_bytes());
        let mut s = Screen::new(46, 79);
        for (op, data) in [
            (Some((3, 39)), &b""[..]),
            (None, b"\x1b[5;10r"),
            (Some((11, 53)), b""),
            (None, "漢字🙂e\u{301}abc\u{200d}abc\x1b[?1049h🙂\x1b7\x1b[s\x1b[168;2L\x1b[?1049l".as_bytes()),
            (Some((51, 18)), b""),
            (None, "漢字🙂e\u{301}漢字漢字\u{200d}".as_bytes()),
            (Some((4, 170)), b""),
            (None, b"\x1b[216;20T\x1b8e\xcc\x81\x1b7\x1bD"),
            (Some((51, 53)), b""),
            (None, "\x1b[220H\x1b[?1049labc🙂".as_bytes()),
        ] {
            match op {
                Some((r, c)) => s.set_size(r, c),
                None => s.process(data),
            }
        }
        let _ = s.formatted();
    }
}
