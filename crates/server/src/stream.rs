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
        let keep = bytes.len().min(self.max_ring);
        let drop = self.ring.len().saturating_sub(self.max_ring - keep);
        self.ring.drain(..drop);
        self.start += (drop + bytes.len() - keep) as u64;
        self.ring.extend(&bytes[bytes.len() - keep..]);
        while self.markers.front().is_some_and(|m| m.offset < self.start) {
            self.markers.pop_front();
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

    /// The retained bytes from `offset` up to `end` or the stream's end,
    /// or `None` when the stream does not hold `offset`.
    pub fn read(&self, offset: u64, end: u64) -> Option<Vec<u8>> {
        let (first, second) = self.read_slices(offset, end)?;
        Some([first, second].concat())
    }

    /// A retained range as the ring's (possibly wrapped) borrowed slices.
    pub fn read_slices(&self, offset: u64, end: u64) -> Option<(&[u8], &[u8])> {
        if offset < self.start || offset > self.offset() {
            return None;
        }
        let skip = self.index(offset);
        let end = self.index(end.clamp(offset, self.offset()));
        let (first, second) = self.ring.as_slices();
        Some((
            &first[skip.min(first.len())..end.min(first.len())],
            &second[skip.saturating_sub(first.len())..end.saturating_sub(first.len())],
        ))
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
        self.ring.range(self.index(offset)..).copied()
    }

    /// The ring index of a retained `offset`.
    fn index(&self, offset: u64) -> usize {
        usize::try_from(offset - self.start).expect("offset within the ring")
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
        assert_eq!(s.read(4, 6).unwrap(), b"ef");
        assert_eq!(s.read(4, 0).unwrap(), b"", "an end before the offset");
        assert!(s.read(2, 3).is_none());
        assert!(s.read(8, 9).is_none());
        assert_eq!(s.read(7, 8).unwrap(), b"");
        assert!(
            s.markers_retained_after(0),
            "marker at offset 3 is at the ring start"
        );
        s.append(b"h");
        assert!(!s.markers_retained_after(0));
        assert!(s.markers_retained_after(1));
    }

    #[test]
    fn borrowed_ranges_cross_the_ring_wrap_with_exact_offsets() {
        let mut s = Stream::new(8, 8);
        s.append(b"abcdefgh");
        s.append(b"ijklm");
        let (first, second) = s.read_slices(5, 13).unwrap();
        assert!(!first.is_empty() && !second.is_empty());
        for offset in 5..=13_u64 {
            for len in [0, 1, 3, 8, u64::MAX] {
                let end = offset.saturating_add(len);
                let (first, second) = s.read_slices(offset, end).unwrap();
                let index = |at: u64| usize::try_from(at - 5).unwrap();
                let expected = &b"fghijklm"[index(offset)..index(end.min(13))];
                let frame = proto::encode_output_frame(1, offset, first, second);
                let (_, payload) = proto::parse_frame(&frame, 1024).unwrap();
                assert_eq!(proto::parse_output(payload), Some((offset, expected)));
            }
        }
        assert!(s.read_slices(4, 5).is_none());
        assert!(s.read_slices(14, 15).is_none());
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

    #[test]
    fn full_ring_reuses_its_allocation() {
        let limit = 4 * 1024 * 1024;
        let mut s = Stream::new(limit, 8);
        s.append(&vec![b'x'; limit]);
        let capacity = s.ring.capacity();
        s.append(&[b'y'; 24]);
        assert_eq!(s.ring.capacity(), capacity);
        assert_eq!(s.retained_start(), 24);
        assert_eq!(s.offset(), (limit + 24) as u64);
        assert_eq!(s.read(limit as u64, s.offset()).unwrap(), [b'y'; 24]);
    }

    #[test]
    fn oversized_appends_keep_suffix_and_lifetime_offsets() {
        for limit in [0, 1, 3, 8] {
            let mut s = Stream::new(limit, 8);
            s.append(b"old");
            s.add_marker(MarkerKind::Resized { cols: 1, rows: 1 });
            let bytes = b"0123456789";
            s.append(bytes);
            assert_eq!(s.offset(), 13);
            assert_eq!(s.retained_start(), 13 - limit as u64);
            assert_eq!(
                s.read(s.retained_start(), s.offset()).unwrap(),
                bytes[10 - limit..]
            );
            assert!(!s.markers_retained_after(0));
            s.add_marker(MarkerKind::Resized { cols: 2, rows: 2 });
            s.append(b"");
            assert!(s.markers_retained_after(1));
            assert_eq!(s.offset(), 13);
            s.append(b"z");
            assert_eq!(s.offset(), 14);
            assert_eq!(s.retained_start(), 14 - limit as u64);
            assert_eq!(s.markers_retained_after(1), limit != 0);
        }
    }
}
