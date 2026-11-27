//! `swarmctl dump --html` and `swarmctl record`: a session's output as a web
//! page with its colors, or as an asciinema recording (asciicast v2).

use std::fmt::Write as _;

/// Terminal output as an HTML page: text with its colors, bold, italic and
/// underline; other escape sequences, carriage returns and control or
/// invisible characters are dropped.
pub fn to_html(bytes: &[u8], title: &str) -> String {
    let mut conv = HtmlConverter::default();
    conv.feed(bytes);
    let body = conv.finish();
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{t}</title>\n\
         <style>body{{margin:0;background:#1b1b1d;color:#e6e6e6}}\
         pre{{margin:0;padding:16px;font:13px/1.35 ui-monospace,SFMono-Regular,Menlo,monospace;white-space:pre-wrap}}</style>\n\
         </head><body><pre>{body}</pre></body></html>\n",
        t = escape(title)
    )
}

#[derive(Default, Clone, PartialEq)]
struct Style {
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Style {
    fn css(&self) -> String {
        let (fg, bg) = if self.inverse {
            (Some(self.bg.clone().unwrap_or_else(|| "#1b1b1d".into())), Some(self.fg.clone().unwrap_or_else(|| "#e6e6e6".into())))
        } else {
            (self.fg.clone(), self.bg.clone())
        };
        let mut css = String::new();
        if let Some(c) = fg {
            let _ = write!(css, "color:{c};");
        }
        if let Some(c) = bg {
            let _ = write!(css, "background:{c};");
        }
        if self.bold {
            css.push_str("font-weight:bold;");
        }
        if self.italic {
            css.push_str("font-style:italic;");
        }
        if self.underline {
            css.push_str("text-decoration:underline;");
        }
        css
    }
}

#[derive(Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Text,
    Escape,
    Csi,
    Str,
    StrEscape,
}

#[derive(Default)]
struct HtmlConverter {
    state: State,
    params: String,
    style: Style,
    /// Text bytes not yet written, in the current style.
    text: Vec<u8>,
    out: String,
    open: bool,
}

impl HtmlConverter {
    fn feed(&mut self, bytes: &[u8]) {
        use State::*;
        for &b in bytes {
            self.state = match (self.state, b) {
                (Text, 0x1b) => Escape,
                (Text, b'\n' | b'\t') => {
                    self.text.push(b);
                    Text
                }
                (Text, b) if b < 0x20 || b == 0x7f => Text,
                (Text, b) => {
                    self.text.push(b);
                    Text
                }
                (Escape, b'[') => {
                    self.params.clear();
                    Csi
                }
                (Escape, b']' | b'P' | b'_' | b'^' | b'X') => Str,
                (Escape, _) => Text,
                (Csi, 0x40..=0x7e) => {
                    if b == b'm' {
                        let params = std::mem::take(&mut self.params);
                        self.sgr(&params);
                    }
                    Text
                }
                (Csi, b) => {
                    if self.params.len() < 256 {
                        self.params.push(b as char);
                    }
                    Csi
                }
                (Str, 0x07) => Text,
                (Str, 0x1b) => StrEscape,
                (Str, _) => Str,
                (StrEscape, b'\\') => Text,
                (StrEscape, 0x1b) => StrEscape,
                (StrEscape, _) => Str,
            };
        }
    }

    /// Writes the pending text in the current style.
    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&std::mem::take(&mut self.text)).into_owned();
        let text: String =
            text.chars().filter(|&c| c == '\n' || c == '\t' || !swarm_proto::is_unsafe_char(c)).collect();
        let css = self.style.css();
        if !css.is_empty() {
            let _ = write!(self.out, "<span style=\"{css}\">");
        }
        self.out.push_str(&escape(&text));
        if !css.is_empty() {
            self.out.push_str("</span>");
        }
        self.open = false;
    }

    fn sgr(&mut self, params: &str) {
        // Text written so far keeps the style it had.
        self.flush_text();
        let codes: Vec<u16> = if params.is_empty() {
            vec![0]
        } else {
            params.split([';', ':']).map(|p| p.parse().unwrap_or(0)).collect()
        };
        let mut i = 0;
        while i < codes.len() {
            let s = &mut self.style;
            match codes[i] {
                0 => *s = Style::default(),
                1 => s.bold = true,
                3 => s.italic = true,
                4 => s.underline = true,
                7 => s.inverse = true,
                22 => s.bold = false,
                23 => s.italic = false,
                24 => s.underline = false,
                27 => s.inverse = false,
                c @ 30..=37 => s.fg = Some(palette(c - 30)),
                39 => s.fg = None,
                c @ 40..=47 => s.bg = Some(palette(c - 40)),
                49 => s.bg = None,
                c @ 90..=97 => s.fg = Some(palette(c - 90 + 8)),
                c @ 100..=107 => s.bg = Some(palette(c - 100 + 8)),
                c @ (38 | 48) => {
                    let color = match codes.get(i + 1) {
                        Some(5) => {
                            let n = codes.get(i + 2).copied().unwrap_or(0);
                            i += 2;
                            Some(palette(n.min(255)))
                        }
                        Some(2) => {
                            let at = |k: usize| codes.get(i + k).copied().unwrap_or(0).min(255);
                            let rgb = format!("#{:02x}{:02x}{:02x}", at(2), at(3), at(4));
                            i += 4;
                            Some(rgb)
                        }
                        _ => None,
                    };
                    if c == 38 {
                        s.fg = color;
                    } else {
                        s.bg = color;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn finish(mut self) -> String {
        self.flush_text();
        let _ = self.open;
        self.out
    }
}

/// xterm's 256 colors.
fn palette(n: u16) -> String {
    const BASE: [&str; 16] = [
        "#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5", "#666666", "#f14c4c",
        "#23d18b", "#f5f543", "#3b8eea", "#d670d6", "#29b8db", "#ffffff",
    ];
    match n {
        0..=15 => BASE[n as usize].to_string(),
        16..=231 => {
            let n = n - 16;
            let level = |v: u16| if v == 0 { 0 } else { 55 + v * 40 };
            format!("#{:02x}{:02x}{:02x}", level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let v = 8 + (n - 232) * 10;
            format!("#{v:02x}{v:02x}{v:02x}")
        }
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Output as UTF-8 text for asciicast events: a character cut off at the end
/// of a chunk waits for the next one.
#[derive(Default)]
pub struct Utf8Chunks {
    carry: Vec<u8>,
}

impl Utf8Chunks {
    pub fn push(&mut self, data: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.carry);
        bytes.extend_from_slice(data);
        match std::str::from_utf8(&bytes) {
            Ok(s) => s.to_string(),
            Err(e) if e.error_len().is_none() => {
                let (good, rest) = bytes.split_at(e.valid_up_to());
                self.carry = rest.to_vec();
                String::from_utf8_lossy(good).into_owned()
            }
            Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
        }
    }
}

/// The asciicast v2 header line.
pub fn cast_header(cols: u16, rows: u16, title: &str, unix_secs: u64) -> String {
    serde_json::json!({
        "version": 2,
        "width": cols,
        "height": rows,
        "timestamp": unix_secs,
        "title": title,
        "env": { "TERM": "xterm-256color" },
    })
    .to_string()
}

/// One asciicast output event line.
pub fn cast_event(seconds: f64, text: &str) -> String {
    serde_json::json!([(seconds * 1_000_000.0).round() / 1_000_000.0, "o", text]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_become_spans_and_other_escapes_go() {
        let html = to_html(b"\x1b[1;31merr\x1b[0m <ok>\r\n\x1b]0;title\x07\x1b[38;5;21mblue\x1b[m\x1b[2J", "t");
        assert!(html.contains("<span style=\"color:#cd3131;font-weight:bold;\">err</span> &lt;ok&gt;\n"), "{html}");
        assert!(html.contains("<span style=\"color:#0000ff;\">blue</span>"), "{html}");
        assert!(!html.contains("title\x07") && !html.contains("2J"));
    }

    #[test]
    fn truecolor_and_hostile_text() {
        let html = to_html(b"\x1b[48;2;1;2;3mx\x1b[0m\xe2\x80\xaey</pre><script>", "a<b");
        assert!(html.contains("background:#010203;"));
        assert!(html.contains("y&lt;/pre&gt;&lt;script&gt;"), "{html}");
        assert!(!html.contains('\u{202e}'), "bidi overrides are dropped");
        assert!(html.contains("<title>a&lt;b</title>"));
    }

    #[test]
    fn cast_lines_and_split_characters() {
        let mut u = Utf8Chunks::default();
        assert_eq!(u.push(b"a\xc3"), "a");
        assert_eq!(u.push(b"\xa9"), "\u{e9}");
        assert_eq!(cast_event(1.5, "hi\n"), r#"[1.5,"o","hi\n"]"#);
        assert!(cast_header(80, 24, "dev", 1).starts_with(r#"{"version":2,"width":80,"height":24"#));
    }
}
