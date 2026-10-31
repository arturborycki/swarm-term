//! Scrollback ring buffer addressed by absolute byte offsets.

use std::collections::VecDeque;

pub struct Ring {
    data: VecDeque<u8>,
    /// Offset of `data[0]`.
    start: u64,
    capacity: usize,
    /// Offsets where replay can safely start, ascending.
    boundaries: VecDeque<u64>,
}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        let mut boundaries = VecDeque::new();
        boundaries.push_back(0);
        Ring { data: VecDeque::with_capacity(capacity.min(1 << 20)), start: 0, capacity, boundaries }
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.capacity {
            let keep = &bytes[bytes.len() - self.capacity..];
            self.start = self.end() + (bytes.len() - self.capacity) as u64;
            self.data.clear();
            self.data.extend(keep);
        } else {
            let overflow = (self.data.len() + bytes.len()).saturating_sub(self.capacity);
            if overflow > 0 {
                self.data.drain(..overflow);
                self.start += overflow as u64;
            }
            self.data.extend(bytes);
        }
        while self.boundaries.front().is_some_and(|&b| b < self.start) {
            self.boundaries.pop_front();
        }
    }

    pub fn add_boundary(&mut self, offset: u64) {
        if offset < self.start || offset > self.end() {
            return;
        }
        if self.boundaries.back().is_some_and(|&b| b >= offset) {
            return;
        }
        self.boundaries.push_back(offset);
        // Bound memory: keep the most recent boundaries; replay prefers the
        // oldest one, so thin out the middle instead of dropping the front.
        if self.boundaries.len() > 4096 {
            let front = self.boundaries.pop_front();
            self.boundaries.remove(0);
            if let Some(f) = front {
                self.boundaries.push_front(f);
            }
        }
    }

    /// Where a full replay starts: the oldest boundary still buffered, or the
    /// start of the buffer if output has wrapped past every boundary.
    pub fn replay_start(&self) -> u64 {
        self.boundaries.front().copied().unwrap_or(self.start)
    }

    /// The offset just after the first `\n` in `from..until`, or `from` if
    /// there is none (so a replay starting there begins on a whole line).
    pub fn next_line_start(&self, from: u64, until: u64) -> u64 {
        if from < self.start || from >= until {
            return from;
        }
        let begin = (from - self.start) as usize;
        let end = ((until.min(self.end())) - self.start) as usize;
        self.data
            .range(begin..end)
            .position(|&b| b == b'\n')
            .map(|i| from + i as u64 + 1)
            .unwrap_or(from)
    }

    pub fn contains(&self, offset: u64) -> bool {
        offset >= self.start && offset <= self.end()
    }

    /// Bytes in `start..end`, if all of them are still buffered.
    pub fn slice(&self, start: u64, end: u64) -> Option<Vec<u8>> {
        if start < self.start || end > self.end() || start > end {
            return None;
        }
        let from = (start - self.start) as usize;
        let to = (end - self.start) as usize;
        Some(self.data.range(from..to).copied().collect())
    }

    /// The replay from `from` to the end as (offset, bytes) pieces. A block
    /// (the bytes between two boundaries) with more than `max_lines` lines
    /// keeps its first `keep_head` bytes (its prompt and command hooks) and
    /// its last `max_lines` lines; the lines in between are left out, cut at
    /// line starts. Terminals keep a limited number of rows per block anyway,
    /// so this saves parsing output they would drop.
    pub fn replay_pieces(&self, from: u64, keep_head: usize, max_lines: usize) -> Vec<(u64, Vec<u8>)> {
        let end = self.end();
        let from = from.max(self.start);
        let mut cuts: Vec<u64> = self.boundaries.iter().copied().filter(|&b| b > from && b < end).collect();
        cuts.push(end);
        let mut pieces: Vec<(u64, Vec<u8>)> = Vec::new();
        let push = |pieces: &mut Vec<(u64, Vec<u8>)>, start: u64, stop: u64| {
            if start >= stop {
                return;
            }
            let bytes = self.slice(start, stop).unwrap_or_default();
            // Merge with the previous piece when contiguous.
            match pieces.last_mut() {
                Some((off, prev)) if *off + prev.len() as u64 == start => prev.extend(bytes),
                _ => pieces.push((start, bytes)),
            }
        };
        let mut start = from;
        for stop in cuts {
            match self.tail_lines_start(start, stop, max_lines) {
                Some(tail) if tail > start + keep_head as u64 => {
                    let head_end = self.next_line_start(start + keep_head as u64, tail);
                    let head_end = if head_end == start + keep_head as u64 { start + keep_head as u64 } else { head_end };
                    push(&mut pieces, start, head_end.min(tail));
                    push(&mut pieces, tail, stop);
                }
                _ => push(&mut pieces, start, stop),
            }
            start = stop;
        }
        pieces
    }

    /// Where the last `max_lines` lines of `start..stop` begin, if it has more.
    fn tail_lines_start(&self, start: u64, stop: u64, max_lines: usize) -> Option<u64> {
        let begin = (start - self.start) as usize;
        let end = (stop - self.start) as usize;
        let mut lines = 0;
        for (i, &b) in self.data.range(begin..end).enumerate().rev() {
            if b == b'\n' && begin + i + 1 < end {
                lines += 1;
                if lines == max_lines {
                    return Some(start + i as u64 + 1);
                }
            }
        }
        None
    }

    /// Bytes from `offset` to the end. `offset` must satisfy `contains`.
    #[cfg(test)]
    pub fn slice_from(&self, offset: u64) -> Vec<u8> {
        let skip = (offset.saturating_sub(self.start)) as usize;
        self.data.range(skip.min(self.data.len())..).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_pieces_cut_the_middle_of_long_blocks() {
        let mut r = Ring::new(1 << 20);
        r.push(b"PROMPT$ cmd\n");
        for i in 0..100 {
            r.push(format!("line {i}\n").as_bytes());
        }
        let b = r.end();
        r.add_boundary(b);
        r.push(b"PROMPT$ next\n");
        let pieces = r.replay_pieces(0, 8, 5);
        let text: Vec<String> = pieces.iter().map(|(_, b)| String::from_utf8_lossy(b).into_owned()).collect();
        assert_eq!(text[0], "PROMPT$ cmd\n", "head kept up to the next line start");
        assert!(text[1].starts_with("line 95\n"), "then the last lines: {:?}", text[1]);
        assert!(text[1].ends_with("PROMPT$ next\n"), "and the next (short) block whole");
        // Offsets are the real ones, so the pieces line up with the ring.
        for (off, bytes) in &pieces {
            assert_eq!(r.slice(*off, off + bytes.len() as u64).unwrap(), *bytes);
        }
        assert_eq!(pieces.last().map(|(o, b)| o + b.len() as u64), Some(r.end()));
        // Short replays come back whole.
        assert_eq!(r.replay_pieces(b, 8, 5), vec![(b, b"PROMPT$ next\n".to_vec())]);
    }

    #[test]
    fn wraps_and_tracks_offsets() {
        let mut r = Ring::new(8);
        r.push(b"hello");
        assert_eq!((r.start(), r.end()), (0, 5));
        r.push(b"world!");
        assert_eq!((r.start(), r.end()), (3, 11));
        assert_eq!(r.slice_from(3), b"loworld!");
        assert_eq!(r.slice_from(9), b"d!");
        assert!(!r.contains(2));
    }

    #[test]
    fn huge_push_keeps_tail() {
        let mut r = Ring::new(4);
        r.push(b"ab");
        r.push(b"0123456789");
        assert_eq!((r.start(), r.end()), (8, 12));
        assert_eq!(r.slice_from(8), b"6789");
    }

    #[test]
    fn replay_starts_at_oldest_buffered_boundary() {
        let mut r = Ring::new(10);
        assert_eq!(r.replay_start(), 0);
        r.push(b"aaaa");
        r.add_boundary(4);
        r.push(b"bbbbbb");
        r.add_boundary(10);
        assert_eq!(r.replay_start(), 0);
        r.push(b"cc");
        // Boundary 0 evicted; 4 is the oldest still buffered.
        assert_eq!(r.replay_start(), 4);
        r.push(b"dddddddd");
        assert_eq!(r.replay_start(), 10);
        r.push(b"eeeee");
        // Every boundary evicted: fall back to buffer start.
        assert_eq!(r.replay_start(), r.start());
    }
}
