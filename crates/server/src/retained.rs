//! The events of a server incarnation, numbered from 1, and the latest of
//! them kept as sent for `subscribe {after}`.

use std::collections::VecDeque;

pub struct Retained {
    /// The `seq` of the latest event.
    seq: u64,
    /// The latest events as sent, oldest first.
    events: VecDeque<(u64, Vec<u8>)>,
    bytes: usize,
    max_events: u64,
    max_bytes: u64,
}

/// A `subscribe {after}` in the server's incarnation.
pub enum Replay<'a> {
    /// `after` is above the latest event.
    Ahead,
    /// Events after `after` are no longer retained.
    Gap,
    /// The retained events above `after`, in order.
    Events(Vec<(u64, &'a [u8])>),
}

impl Retained {
    /// Retains at most `max_events` events and `max_bytes` bytes of them.
    pub fn new(max_events: u64, max_bytes: u64) -> Self {
        Retained {
            seq: 0,
            events: VecDeque::new(),
            bytes: 0,
            max_events,
            max_bytes,
        }
    }

    /// Numbers the next event: its `seq`.
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Retains the frame of the event `next_seq` numbered last, then
    /// drops the oldest while more than the limits are retained.
    pub fn retain(&mut self, frame: Vec<u8>) {
        self.bytes += frame.len();
        self.events.push_back((self.seq, frame));
        while self.events.len() as u64 > self.max_events || self.bytes as u64 > self.max_bytes {
            let Some((_, dropped)) = self.events.pop_front() else {
                break;
            };
            self.bytes -= dropped.len();
        }
    }

    /// `oldest` and `latest` of the `subscribe` reply.
    pub fn cursor(&self) -> (u64, u64) {
        let oldest = self.events.front().map_or(self.seq + 1, |(seq, _)| *seq);
        (oldest, self.seq)
    }

    /// What `subscribe {after}` replays in this incarnation.
    pub fn replay(&self, after: u64) -> Replay<'_> {
        let (oldest, latest) = self.cursor();
        if after > latest {
            Replay::Ahead
        } else if after + 1 < oldest {
            Replay::Gap
        } else {
            Replay::Events(
                self.events
                    .iter()
                    .filter(|(seq, _)| *seq > after)
                    .map(|(seq, frame)| (*seq, frame.as_slice()))
                    .collect(),
            )
        }
    }
}
