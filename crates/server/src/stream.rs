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

/// A place in the stream: the next byte to receive and the latest marker
/// already reflected.
#[derive(Clone, Copy)]
pub struct Cursor {
    pub offset: u64,
    pub marker: u64,
}

/// What a cursor is served.
pub enum Served<'a> {
    Marker(&'a Marker),
    /// Bytes as the ring's (possibly wrapped) slices, and the offset of the
    /// first.
    Output(u64, &'a [u8], &'a [u8]),
}

pub struct Stream {
    ring: VecDeque<u8>,
    /// Offset of the first retained byte.
    start: u64,
    markers: VecDeque<Marker>,
    /// The offset of the latest marker dropped for `max_markers`.
    dropped: Option<u64>,
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
            dropped: None,
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
            self.dropped = self.markers.pop_front().map(|m| m.offset);
        }
    }

    /// Up to `max` retained bytes from `offset`, or `None` when the
    /// stream no longer retains `offset`.
    pub fn read(&self, offset: u64, max: usize) -> Option<Vec<u8>> {
        let (first, second) = self.read_slices(offset, max)?;
        Some([first, second].concat())
    }

    /// A retained range as the ring's (possibly wrapped) borrowed slices.
    pub fn read_slices(&self, offset: u64, max: usize) -> Option<(&[u8], &[u8])> {
        let skip = usize::try_from(offset.checked_sub(self.start)?).ok()?;
        if skip > self.ring.len() {
            return None;
        }
        let end = skip.saturating_add(max).min(self.ring.len());
        let (first, second) = self.ring.as_slices();
        Some((
            &first[skip.min(first.len())..end.min(first.len())],
            &second[skip.saturating_sub(first.len())..end.saturating_sub(first.len())],
        ))
    }

    /// For an offset within the ring, the latest marker `seq` at or before
    /// `offset`, as a cursor at `offset` reflects it; 0 when a marker after
    /// `offset` was dropped for `max_markers`, and which came before it is
    /// unknown.
    pub fn seq_at(&self, offset: u64) -> u64 {
        if self.dropped.is_some_and(|d| d > offset) {
            return 0;
        }
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

    /// Whether the stream can serve `cursor`: its offset is retained, its
    /// marker is at most the latest, and every marker above it is
    /// retained.
    pub fn retains(&self, cursor: Cursor) -> bool {
        self.start <= cursor.offset
            && cursor.offset <= self.offset()
            && cursor.marker <= self.latest_seq
            && self.markers_retained_after(cursor.marker)
    }

    /// The stream from `cursor`, which it retains: each marker before any
    /// byte at or past its offset, the bytes in pieces of at most `chunk`
    /// and up to `budget` in all. `cursor` advances past what is returned.
    pub fn serve(&self, cursor: &mut Cursor, chunk: usize, mut budget: usize) -> Vec<Served<'_>> {
        debug_assert!(self.retains(*cursor));
        let end = self.offset();
        let mut served = Vec::new();
        let mut markers = self.markers_after(cursor.marker).peekable();
        loop {
            while let Some(m) = markers.next_if(|m| m.offset <= cursor.offset) {
                served.push(Served::Marker(m));
                cursor.marker = m.seq;
            }
            if cursor.offset >= end || budget == 0 {
                break;
            }
            let limit = markers
                .peek()
                .map_or(end, |m| m.offset)
                .min(end)
                .min(cursor.offset + chunk.min(budget) as u64);
            let (first, second) = self
                .read_slices(cursor.offset, (limit - cursor.offset) as usize)
                .expect("offset within the retained stream");
            let len = first.len() + second.len();
            budget -= len;
            served.push(Served::Output(cursor.offset, first, second));
            cursor.offset += len as u64;
        }
        served
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_at_is_0_past_a_marker_dropped_for_the_count() {
        let mut s = Stream::new(3, 1);
        s.append(b"a");
        s.add_marker(MarkerKind::Resized { cols: 1, rows: 1 });
        s.add_marker(MarkerKind::Resized { cols: 2, rows: 1 });
        assert_eq!(s.seq_at(0), 0, "marker 1, after offset 0, is gone");
        assert_eq!(s.seq_at(1), 2);
    }

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
    fn borrowed_ranges_cross_the_ring_wrap_with_exact_offsets() {
        let mut s = Stream::new(8, 8);
        s.append(b"abcdefgh");
        s.append(b"ijklm");
        let (first, second) = s.read_slices(5, 8).unwrap();
        assert!(!first.is_empty() && !second.is_empty());
        for offset in 5..=13 {
            for max in [0, 1, 3, 8, usize::MAX] {
                let (first, second) = s.read_slices(offset, max).unwrap();
                let expected = &b"fghijklm"[(offset - 5) as usize..];
                let expected = &expected[..max.min(expected.len())];
                let frame = proto::encode_output_frame(1, offset, first, second);
                let (_, payload) = proto::parse_frame(&frame, 1024).unwrap();
                assert_eq!(proto::parse_output(payload), Some((offset, expected)));
            }
        }
        assert!(s.read_slices(4, 1).is_none());
        assert!(s.read_slices(14, 1).is_none());
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
        assert_eq!(s.read(limit as u64, 24).unwrap(), [b'y'; 24]);
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
                s.read(s.retained_start(), 100).unwrap(),
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
