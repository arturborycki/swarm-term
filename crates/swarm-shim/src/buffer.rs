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

    /// Bytes from `offset` to the end. `offset` must satisfy `contains`.
    pub fn slice_from(&self, offset: u64) -> Vec<u8> {
        let skip = (offset.saturating_sub(self.start)) as usize;
        self.data.range(skip.min(self.data.len())..).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
