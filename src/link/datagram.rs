//! Datagrams, the way UDP (and WebTransport's datagrams) treat them: each
//! one on its own. Lost is gone, jitter can reorder, a full bottleneck queue
//! drops the newest, and now and then one arrives twice.

use core::{cmp::Reverse, time::Duration};
use std::collections::BinaryHeap;

use super::{HEADER_BYTES, Path, Stats};

pub(super) struct Datagram<T> {
    flying: BinaryHeap<Reverse<Flying<T>>>,
    seq: u64,
    bytes: usize,
}

struct Flying<T> {
    arrival: Duration,
    seq: u64,
    sent: Duration,
    bytes: usize,
    item: T,
}

impl<T> Default for Datagram<T> {
    fn default() -> Self {
        Self {
            flying: BinaryHeap::new(),
            seq: 0,
            bytes: 0,
        }
    }
}

impl<T: Clone> Datagram<T> {
    pub(super) fn push(&mut self, path: &mut Path, stats: &mut Stats, now: Duration, item: T, len: usize) {
        if path.lost(now) {
            stats.lost += 1;
            return;
        }
        if path.backlog(now) > path.config.queue {
            stats.dropped += 1;
            return;
        }
        let departure = path.depart(now, len);
        let copies = if path.rng.chance(path.config.duplicate / 100.0) {
            stats.duplicated += 1;
            2
        } else {
            1
        };
        for _ in 0..copies {
            let delay = if path.rng.chance(path.config.reorder / 100.0) {
                Duration::ZERO
            } else {
                path.delay()
            };
            self.seq += 1;
            self.bytes += len + HEADER_BYTES;
            self.flying.push(Reverse(Flying {
                arrival: departure + delay,
                seq: self.seq,
                sent: now,
                bytes: len + HEADER_BYTES,
                item: item.clone(),
            }));
        }
    }
}

impl<T> Datagram<T> {
    /// The earliest arrival, once it is due: `(sent, arrived, item)`.
    pub(super) fn pop(&mut self, now: Duration) -> Option<(Duration, Duration, T)> {
        if self.flying.peek()?.0.arrival > now {
            return None;
        }
        let Reverse(flying) = self.flying.pop()?;
        self.bytes -= flying.bytes;
        Some((flying.sent, flying.arrival, flying.item))
    }

    /// `(packets in flight, their bytes, no stall)`.
    pub(super) fn held(&self) -> (usize, usize, Duration) {
        (self.flying.len(), self.bytes, Duration::ZERO)
    }

    pub(super) fn drain(self) -> Vec<T> {
        let mut flying = self.flying.into_vec();
        flying.sort_by_key(|Reverse(flying)| (flying.arrival, flying.seq));
        flying.into_iter().map(|Reverse(flying)| flying.item).collect()
    }
}

impl<T> PartialEq for Flying<T> {
    fn eq(&self, other: &Self) -> bool {
        (self.arrival, self.seq) == (other.arrival, other.seq)
    }
}

impl<T> Eq for Flying<T> {}

impl<T> PartialOrd for Flying<T> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Flying<T> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        (self.arrival, self.seq).cmp(&(other.arrival, other.seq))
    }
}
