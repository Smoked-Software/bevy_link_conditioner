//! A byte stream the way Linux TCP delivers one to a game: in order, never
//! lost, and stalled whole behind each loss until the retransmission lands.
//!
//! Each packet is one segment (Nagle off), and ACKs are never lost. Fitted
//! to real TCP under netem (see the README for the numbers):
//!
//! - A loss is found RACK-style: once any later segment is acknowledged, the
//!   lost one goes again a quarter RTT after its own ack was due, so a loss
//!   at `t` is resent at `max(next send + RTT, t + 1.25 RTT)`, later with
//!   jitter. A lost retransmission is found the same way while data flows.
//! - With nothing flowing to reveal it, a tail loss probe goes out after two
//!   smoothed RTTs, then the retransmission timer (SRTT + 200 ms) fires,
//!   doubling each time. After a timeout the window restarts at one segment
//!   and slow-starts, so a stream back from an outage drains its backlog 1,
//!   2, 4... segments per round trip.
//! - The window starts at Linux's 10 segments and grows while full, which a
//!   thin game stream rarely is; only a timeout cuts it. Past ~150 ms RTT
//!   real TCP runs a little worse, because the cut it makes on every loss
//!   starts to hold back sends.

use core::{cmp::Reverse, time::Duration};
use std::collections::{BinaryHeap, VecDeque};

use super::{HEADER_BYTES, Path, Stats};

/// Linux's floor on the retransmission timeout above the smoothed RTT.
const RTO_MIN: Duration = Duration::from_millis(200);
/// Linux doubles up to two minutes; this stops a little before.
const MAX_BACKOFF: u32 = 8;
/// Linux's floor on the tail loss probe timeout.
const TLP_MIN: Duration = Duration::from_millis(10);
/// Linux's initial congestion window, in segments.
const INITIAL_WINDOW: f64 = 10.0;

pub(super) struct Stream<T> {
    /// Every segment not yet delivered, in order; the front is `first`.
    segments: VecDeque<Segment<T>>,
    first: u64,
    /// The first segment not yet put on the wire.
    unsent: u64,
    /// Segments put on the wire and not yet acknowledged, in order.
    flights: VecDeque<Flight>,
    /// Bytes the sender holds: unsent and unacknowledged.
    sender_bytes: usize,
    acks: BinaryHeap<Reverse<Ack>>,
    srtt: Option<Duration>,
    /// The time of the last event played.
    clock: Duration,
    /// The last acknowledgement or timeout, which the timers count from.
    last_progress: Duration,
    probed: bool,
    backoff: u32,
    /// The congestion window, in segments.
    cwnd: f64,
    /// When the last segment was handed to the application.
    delivered: Duration,
}

struct Segment<T> {
    item: T,
    sent: Duration,
    bytes: usize,
    arrival: Option<Duration>,
    /// A transmission of it was lost: whatever it holds up is a stall.
    hit: bool,
}

struct Flight {
    seq: u64,
    bytes: usize,
    sent: Duration,
    lost: bool,
    retransmitted: bool,
    /// When it goes again, once the sender counts it lost: a later segment's
    /// ack showed it missing (RACK), or a timeout gave up on everything.
    due: Option<Duration>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Ack {
    at: Duration,
    seq: u64,
}

enum Event {
    Ack,
    Resend(usize),
    Timer,
}

impl<T> Default for Stream<T> {
    fn default() -> Self {
        Self {
            segments: VecDeque::new(),
            first: 0,
            unsent: 0,
            flights: VecDeque::new(),
            sender_bytes: 0,
            acks: BinaryHeap::new(),
            srtt: None,
            clock: Duration::ZERO,
            last_progress: Duration::ZERO,
            probed: false,
            backoff: 0,
            cwnd: INITIAL_WINDOW,
            delivered: Duration::ZERO,
        }
    }
}

impl<T> Stream<T> {
    pub(super) fn push(
        &mut self,
        path: &mut Path,
        stats: &mut Stats,
        now: Duration,
        item: T,
        len: usize,
    ) {
        self.advance(path, stats, now);
        let bytes = len + HEADER_BYTES;
        if self.sender_bytes > 0 && self.sender_bytes + bytes > path.config.send_buffer {
            stats.dropped += 1;
            return;
        }
        self.sender_bytes += bytes;
        self.segments.push_back(Segment {
            item,
            sent: now,
            bytes,
            arrival: None,
            hit: false,
        });
        self.send_new(path, stats, now);
    }

    /// The head of the stream, once it has arrived: `(sent, delivered, item)`.
    pub(super) fn pop(
        &mut self,
        path: &mut Path,
        stats: &mut Stats,
        now: Duration,
    ) -> Option<(Duration, Duration, T)> {
        self.advance(path, stats, now);
        let arrival = self
            .segments
            .front()?
            .arrival
            .filter(|&arrival| arrival <= now)?;
        let segment = self.segments.pop_front()?;
        self.first += 1;
        self.delivered = self.delivered.max(arrival);
        Some((segment.sent, self.delivered, segment.item))
    }

    /// `(segments undelivered, bytes held by the sender, stall)`.
    pub(super) fn held(&self, path: &Path, now: Duration) -> (usize, usize, Duration) {
        let stall = self
            .segments
            .front()
            .filter(|head| head.hit)
            .map_or(Duration::ZERO, |head| {
                now.saturating_sub(head.sent + path.config.latency)
            });
        (self.segments.len(), self.sender_bytes, stall)
    }

    pub(super) fn drain(self) -> Vec<T> {
        self.segments
            .into_iter()
            .map(|segment| segment.item)
            .collect()
    }

    /// Plays every acknowledgement, resend and timer due by `now`, in order.
    fn advance(&mut self, path: &mut Path, stats: &mut Stats, now: Duration) {
        loop {
            let ack = self.acks.peek().map(|Reverse(ack)| (ack.at, Event::Ack));
            let resend = self
                .next_resend()
                .map(|(at, index)| (at, Event::Resend(index)));
            let timer = self.timer(path).map(|at| (at, Event::Timer));
            let Some((at, event)) = [ack, resend, timer]
                .into_iter()
                .flatten()
                .min_by_key(|(at, _)| *at)
                .filter(|(at, _)| *at <= now)
            else {
                break;
            };
            self.clock = at;
            match event {
                Event::Ack => {
                    let Reverse(ack) = self.acks.pop().expect("peeked");
                    self.on_ack(path, ack);
                }
                Event::Resend(index) => {
                    stats.retransmits += 1;
                    self.transmit(path, stats, index, at);
                }
                Event::Timer => self.on_timer(path, stats, at),
            }
            self.send_new(path, stats, at);
        }
    }

    /// Segments the sender believes are on the wire: what the window limits.
    fn in_flight(&self) -> usize {
        self.flights
            .iter()
            .filter(|flight| flight.due.is_none())
            .count()
    }

    /// The next resend RACK has scheduled and the window allows.
    fn next_resend(&self) -> Option<(Duration, usize)> {
        if self.in_flight() as f64 >= self.cwnd {
            return None;
        }
        (0..self.flights.len())
            .filter_map(|index| Some((self.flights[index].due?.max(self.clock), index)))
            .min()
    }

    /// The tail loss probe, then the retransmission timer.
    fn timer(&self, path: &Path) -> Option<Duration> {
        if self.flights.is_empty() {
            return None;
        }
        let srtt = self.srtt.unwrap_or_else(|| path.rtt());
        Some(if self.probed {
            self.last_progress + (srtt + RTO_MIN) * (1 << self.backoff.min(MAX_BACKOFF))
        } else {
            self.last_progress + (srtt * 2).max(TLP_MIN)
        })
    }

    fn on_ack(&mut self, path: &Path, ack: Ack) {
        let Ok(index) = self
            .flights
            .binary_search_by_key(&ack.seq, |flight| flight.seq)
        else {
            return;
        };
        let flight = self.flights.remove(index).expect("found");
        self.sender_bytes -= flight.bytes;
        if !flight.retransmitted {
            let sample = ack.at.saturating_sub(flight.sent);
            self.srtt = Some(self.srtt.map_or(sample, |srtt| (srtt * 7 + sample) / 8));
        }
        self.last_progress = ack.at;
        self.probed = false;
        self.backoff = 0;
        if self.in_flight() as f64 + 1.0 >= self.cwnd {
            self.cwnd += 1.0;
        }
        // RACK: this segment's arrival shows every lost one sent before it
        // missing, and lost once a reorder window past its own expected ack.
        let rtt = path.rtt();
        for earlier in &mut self.flights {
            if earlier.lost && earlier.due.is_none() && earlier.sent < flight.sent {
                earlier.due = Some(ack.at.max(earlier.sent + rtt + reorder_window(path)));
            }
        }
    }

    fn on_timer(&mut self, path: &mut Path, stats: &mut Stats, at: Duration) {
        stats.retransmits += 1;
        stats.timeouts += 1;
        if self.probed {
            // The retransmission timeout: everything counts as lost, the head
            // goes again, and the window restarts at one.
            for flight in &mut self.flights {
                flight.due.get_or_insert(at);
            }
            self.transmit(path, stats, 0, at);
            self.backoff += 1;
            self.cwnd = 1.0;
        } else {
            // The tail loss probe: its ack, if it gets through, shows the rest.
            self.probed = true;
            self.transmit(path, stats, self.flights.len() - 1, at);
        }
        self.last_progress = at;
    }

    /// Puts new segments on the wire as far as the window allows, after any
    /// resends that are due.
    fn send_new(&mut self, path: &mut Path, stats: &mut Stats, at: Duration) {
        let end = self.first + self.segments.len() as u64;
        while self.unsent < end
            && (self.in_flight() as f64) < self.cwnd
            && !self
                .flights
                .iter()
                .any(|flight| flight.due.is_some_and(|due| due <= at))
        {
            let bytes = self.segments[(self.unsent - self.first) as usize].bytes;
            if self.flights.is_empty() {
                // The timers start with the first segment out.
                self.last_progress = at;
            }
            self.flights.push_back(Flight {
                seq: self.unsent,
                bytes,
                sent: at,
                lost: false,
                retransmitted: false,
                due: None,
            });
            self.unsent += 1;
            self.transmit(path, stats, self.flights.len() - 1, at);
        }
    }

    /// Puts `self.flights[index]` on the wire at `at`, again if it was before.
    fn transmit(&mut self, path: &mut Path, stats: &mut Stats, index: usize, at: Duration) {
        let flight = &mut self.flights[index];
        flight.retransmitted |= flight.sent != at || flight.lost;
        flight.sent = at;
        flight.due = None;
        flight.lost = path.lost(at);
        let segment = flight
            .seq
            .checked_sub(self.first)
            .and_then(|offset| self.segments.get_mut(offset as usize));
        if flight.lost {
            stats.lost += 1;
            if let Some(segment) = segment {
                segment.hit = true;
            }
            return;
        }
        let arrival = path.depart(at, flight.bytes - HEADER_BYTES) + path.delay();
        if let Some(segment) = segment {
            segment.arrival = Some(
                segment
                    .arrival
                    .map_or(arrival, |earlier| earlier.min(arrival)),
            );
        }
        // The receiver acknowledges on arrival; the ACK takes the way back.
        let back = path.rtt().saturating_sub(path.config.latency);
        self.acks.push(Reverse(Ack {
            at: arrival + back,
            seq: flight.seq,
        }));
    }
}

/// How long past its expected ack RACK waits before calling a segment lost:
/// a quarter of the minimum RTT, widened, as Linux widens it on seeing
/// reordering, by three standard deviations of the jitter (fitted to netem
/// runs with 10 to 30 ms of jitter).
fn reorder_window(path: &Path) -> Duration {
    path.rtt() / 4 + path.config.jitter * 3
}
