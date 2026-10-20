//! The per-session output stream: a bounded ring of PTY bytes with
//! lifetime offsets, plus the markers that happened in it.

use std::collections::VecDeque;

use proto::Exit;

pub enum MarkerKind {
    Resized { cols: u16, rows: u16 },
    Exited { exit: Exit },
}

pub struct Marker {
    pub seq: u64,
    pub offset: u64,
    pub kind: MarkerKind,
}

pub struct Stream {
    ring: VecDeque<u8>,
    /// Offset of the first retained byte.
    start: u64,
    markers: VecDeque<Marker>,
    latest_seq: u64,
    max_ring: usize,
    max_markers: usize,
}

impl Stream {
    pub fn new(max_ring: usize, max_markers: usize) -> Self {
        Stream {
            ring: VecDeque::new(),
            start: 0,
            markers: VecDeque::new(),
            latest_seq: 0,
            max_ring,
            max_markers,
        }
    }

    /// Offset of the next byte.
    pub fn offset(&self) -> u64 {
        self.start + self.ring.len() as u64
    }

    pub fn retained_start(&self) -> u64 {
        self.start
    }

    pub fn latest_seq(&self) -> u64 {
        self.latest_seq
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.ring.extend(bytes);
        if self.ring.len() > self.max_ring {
            let drop = self.ring.len() - self.max_ring;
            self.ring.drain(..drop);
            self.start += drop as u64;
            while self.markers.front().is_some_and(|m| m.offset < self.start) {
                self.markers.pop_front();
            }
        }
    }

    pub fn add_marker(&mut self, kind: MarkerKind) {
        self.latest_seq += 1;
        self.markers.push_back(Marker {
            seq: self.latest_seq,
            offset: self.offset(),
            kind,
        });
        if self.markers.len() > self.max_markers {
            self.markers.pop_front();
        }
    }

    /// Up to `max` retained bytes from `offset`, or `None` when the
    /// stream no longer retains `offset`.
    pub fn read(&self, offset: u64, max: usize) -> Option<Vec<u8>> {
        let skip = usize::try_from(offset.checked_sub(self.start)?).ok()?;
        if skip > self.ring.len() {
            return None;
        }
        let end = skip.saturating_add(max).min(self.ring.len());
        Some(self.ring.range(skip..end).copied().collect())
    }

    /// The latest marker `seq` at or before `offset`, as a cursor at
    /// `offset` reflects it.
    pub fn seq_at(&self, offset: u64) -> u64 {
        let later = self.markers.iter().filter(|m| m.offset > offset).count();
        self.latest_seq - later as u64
    }

    /// The bytes from `offset` on, which is retained: from
    /// `retained_start` up to `offset()`.
    pub fn bytes_from(&self, offset: u64) -> impl Iterator<Item = u8> + '_ {
        self.ring.range((offset - self.start) as usize..).copied()
    }

    /// Whether every marker with `seq` above `after` is still retained.
    pub fn markers_retained_after(&self, after: u64) -> bool {
        after >= self.latest_seq || self.markers.front().is_some_and(|m| m.seq <= after + 1)
    }

    pub fn markers_after(&self, after: u64) -> impl Iterator<Item = &Marker> {
        self.markers.iter().filter(move |m| m.seq > after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_drops_oldest_bytes_and_markers() {
        let mut s = Stream::new(4, 8);
        s.append(b"abc");
        s.add_marker(MarkerKind::Resized { cols: 1, rows: 1 });
        let m1 = s.markers_after(0).next().unwrap();
        assert_eq!((m1.seq, m1.offset), (1, 3));
        s.append(b"defg");
        assert_eq!(s.offset(), 7);
        assert_eq!(s.retained_start(), 3);
        assert_eq!(s.read(3, 100).unwrap(), b"defg");
        assert_eq!(s.read(4, 2).unwrap(), b"ef");
        assert!(s.read(2, 1).is_none());
        assert!(s.read(8, 1).is_none());
        assert_eq!(s.read(7, 1).unwrap(), b"");
        assert!(
            s.markers_retained_after(0),
            "marker at offset 3 is at the ring start"
        );
        s.append(b"h");
        assert!(!s.markers_retained_after(0));
        assert!(s.markers_retained_after(1));
    }

    #[test]
    fn marker_count_is_bounded() {
        let mut s = Stream::new(100, 2);
        for i in 0..3 {
            s.add_marker(MarkerKind::Resized { cols: i, rows: 1 });
        }
        assert_eq!(
            s.markers_after(0).map(|m| m.seq).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(!s.markers_retained_after(0));
        assert!(s.markers_retained_after(1));
        assert!(s.markers_retained_after(3));
    }
}
