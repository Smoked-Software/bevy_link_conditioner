//! One direction of a conditioned link, as a plain model: packets go in with
//! the time they were sent, come out when they would have arrived. No Bevy,
//! no clock: time is a [`Duration`] since any start you like, so tests run
//! minutes of traffic in milliseconds and a seed replays the same timeline.

mod datagram;
mod stream;

use core::time::Duration;
use std::collections::VecDeque;

use crate::{Direction, Mode};
use datagram::Datagram;
use stream::Stream;

/// Bytes of headers every packet carries on the wire (IP, TCP or UDP, and
/// a little framing), counted against the bandwidth and the send buffer.
pub const HEADER_BYTES: usize = 40;

/// How far back [`Link::history`] reaches.
pub const HISTORY: Duration = Duration::from_secs(5);

/// One direction of a link: what goes in at `push` comes out of `pop_ready`
/// late, held back, or not at all, as the [`Direction`] and [`Mode`] say.
pub struct Link<T> {
    mode: Mode,
    path: Path,
    model: Model<T>,
    /// Packets let go at once by a change of mode, delivered first.
    released: VecDeque<T>,
    stats: Stats,
    history: VecDeque<(Duration, Duration)>,
}

enum Model<T> {
    Stream(Stream<T>),
    Datagram(Datagram<T>),
}

/// What a link has done so far, and what it holds now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    /// Packets pushed.
    pub sent: u64,
    /// Packets popped.
    pub delivered: u64,
    /// Transmissions lost on the wire. A stream retransmits each one.
    pub lost: u64,
    /// Stream retransmissions, the ones that fired on a timeout included.
    pub retransmits: u64,
    /// Stream retransmissions that fired on a timeout (nothing flowing to
    /// reveal the loss sooner), each one doubling the next wait.
    pub timeouts: u64,
    /// Packets dropped by the sender at a full send buffer (stream) or by a
    /// full bottleneck queue (datagram). Never sent, never retried.
    pub dropped: u64,
    /// Datagrams delivered twice.
    pub duplicated: u64,
    /// Packets held right now.
    pub queued: usize,
    /// Their bytes, headers included.
    pub queued_bytes: usize,
    /// How long the head of a stream has been waiting on a retransmission.
    pub stall: Duration,
    /// One-way delay of the last packet delivered.
    pub last_delay: Duration,
}

impl<T: Clone> Link<T> {
    pub fn new(mode: Mode, config: Direction, seed: u64) -> Self {
        Self {
            mode,
            path: Path::new(config, seed),
            model: Model::new(mode),
            released: VecDeque::new(),
            stats: Stats::default(),
            history: VecDeque::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn config(&self) -> &Direction {
        &self.path.config
    }

    /// Takes new settings live. Packets already in flight keep their fate;
    /// a change of mode lets every held packet go at once.
    pub fn set_config(&mut self, mode: Mode, config: Direction) {
        self.path.config = config;
        if mode != self.mode {
            self.mode = mode;
            let old = core::mem::replace(&mut self.model, Model::new(mode));
            self.released.extend(old.drain());
        }
    }

    /// The emulated path's shortest round trip, which times a stream's loss
    /// detection and retransmissions. Defaults to twice this direction's
    /// latency; set it when the other direction differs or the real link
    /// underneath adds its own.
    pub fn set_round_trip(&mut self, rtt: Duration) {
        self.path.rtt = Some(rtt);
    }

    pub fn round_trip(&self) -> Duration {
        self.path.rtt()
    }

    /// Sends `item` (`len` bytes of payload) at `now`.
    pub fn push(&mut self, now: Duration, item: T, len: usize) {
        self.stats.sent += 1;
        match &mut self.model {
            Model::Stream(stream) => stream.push(&mut self.path, &mut self.stats, now, item, len),
            Model::Datagram(datagram) => datagram.push(&mut self.path, &mut self.stats, now, item, len),
        }
        self.refresh(now);
    }

    /// The next packet that has arrived by `now`, if any.
    pub fn pop_ready(&mut self, now: Duration) -> Option<T> {
        if let Some(item) = self.released.pop_front() {
            self.stats.delivered += 1;
            return Some(item);
        }
        let popped = match &mut self.model {
            Model::Stream(stream) => stream.pop(&mut self.path, &mut self.stats, now),
            Model::Datagram(datagram) => datagram.pop(now),
        };
        if let Some((sent, arrived, _)) = &popped {
            let delay = arrived.saturating_sub(*sent);
            self.stats.delivered += 1;
            self.stats.last_delay = delay;
            self.history.push_back((*arrived, delay));
        }
        self.refresh(now);
        popped.map(|(_, _, item)| item)
    }

    /// Everything held, in order, at once: for switching the link off.
    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        let model = core::mem::replace(&mut self.model, Model::new(self.mode));
        let items: Vec<T> = self.released.drain(..).chain(model.drain()).collect();
        self.stats.delivered += items.len() as u64;
        self.stats.queued = 0;
        self.stats.queued_bytes = 0;
        self.stats.stall = Duration::ZERO;
        items.into_iter()
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// `(arrived at, one-way delay)` of every packet delivered in the last
    /// [`HISTORY`], oldest first.
    pub fn history(&self) -> impl ExactSizeIterator<Item = (Duration, Duration)> + '_ {
        self.history.iter().copied()
    }

    fn refresh(&mut self, now: Duration) {
        let (queued, bytes, stall) = match &self.model {
            Model::Stream(stream) => stream.held(&self.path, now),
            Model::Datagram(datagram) => datagram.held(),
        };
        self.stats.queued = queued + self.released.len();
        self.stats.queued_bytes = bytes;
        self.stats.stall = stall;
        let horizon = now.saturating_sub(HISTORY);
        while self.history.front().is_some_and(|&(at, _)| at < horizon) {
            self.history.pop_front();
        }
    }
}

impl<T> Model<T> {
    fn new(mode: Mode) -> Self {
        match mode {
            Mode::Stream => Self::Stream(Stream::default()),
            Mode::Datagram => Self::Datagram(Datagram::default()),
        }
    }

    fn drain(self) -> Vec<T> {
        match self {
            Self::Stream(stream) => stream.drain(),
            Self::Datagram(datagram) => datagram.drain(),
        }
    }
}

/// The wire under either model: its settings, its weather (bursts and
/// outages, both timed in real time), its jitter and its bottleneck.
struct Path {
    config: Direction,
    rtt: Option<Duration>,
    rng: Rng,
    /// Whether a burst is on, and until when the current spell lasts.
    bad: bool,
    spell_until: Duration,
    /// The last jitter drawn, in standard deviations, for correlation.
    jitter: f64,
    /// When the bottleneck is free to put the next packet on the wire.
    wire_free: Duration,
}

impl Path {
    fn new(config: Direction, seed: u64) -> Self {
        Self {
            config,
            rtt: None,
            rng: Rng::new(seed),
            bad: true,
            spell_until: Duration::ZERO,
            jitter: 0.0,
            wire_free: Duration::ZERO,
        }
    }

    fn rtt(&self) -> Duration {
        self.rtt.unwrap_or(self.config.latency * 2)
    }

    /// Whether a packet put on the wire at `t` is lost. Asked in time order.
    fn lost(&mut self, t: Duration) -> bool {
        let config = &self.config;
        if !config.outage_every.is_zero()
            && t >= config.outage_every
            && t.as_nanos() % config.outage_every.as_nanos() < config.outage_hold.as_nanos()
        {
            return true;
        }
        if config.loss <= 0.0 {
            return false;
        }
        if config.burst.is_zero() {
            return self.rng.chance(config.loss / 100.0);
        }
        let loss = (config.loss / 100.0).min(0.999) as f64;
        while t >= self.spell_until {
            self.bad = !self.bad;
            let mean = if self.bad {
                config.burst.as_secs_f64()
            } else {
                config.burst.as_secs_f64() * (1.0 - loss) / loss
            };
            self.spell_until += Duration::from_secs_f64(self.rng.exponential(mean));
        }
        self.bad
    }

    /// The next packet's one-way delay: latency plus correlated jitter.
    fn delay(&mut self) -> Duration {
        let config = &self.config;
        if config.jitter.is_zero() {
            return config.latency;
        }
        let rho = (config.correlation / 100.0).clamp(0.0, 0.999) as f64;
        self.jitter = rho * self.jitter + (1.0 - rho * rho).sqrt() * self.rng.normal();
        let jitter = self.jitter * config.jitter.as_secs_f64();
        Duration::from_secs_f64((config.latency.as_secs_f64() + jitter).max(0.0))
    }

    /// When a packet of `len` payload bytes handed over at `t` leaves the
    /// bottleneck.
    fn depart(&mut self, t: Duration, len: usize) -> Duration {
        if self.config.bandwidth == 0 {
            return t;
        }
        let departure = t.max(self.wire_free);
        let bits = ((len + HEADER_BYTES) * 8) as f64;
        self.wire_free = departure + Duration::from_secs_f64(bits / (self.config.bandwidth as f64 * 1000.0));
        departure
    }

    /// How long a packet handed over at `t` would wait for the bottleneck.
    fn backlog(&self, t: Duration) -> Duration {
        if self.config.bandwidth == 0 {
            Duration::ZERO
        } else {
            self.wire_free.saturating_sub(t)
        }
    }
}

/// SplitMix64: tiny, seedable, the same on every target.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn chance(&mut self, p: f32) -> bool {
        p > 0.0 && self.unit() < p as f64
    }

    /// Standard normal (Box-Muller).
    fn normal(&mut self) -> f64 {
        let u = 1.0 - self.unit();
        (-2.0 * u.ln()).sqrt() * (core::f64::consts::TAU * self.unit()).cos()
    }

    fn exponential(&mut self, mean: f64) -> f64 {
        -(1.0 - self.unit()).ln() * mean
    }
}

/// Mixes a seed with a salt (a direction, a session) into a new seed.
pub fn mix_seed(seed: u64, salt: u64) -> u64 {
    let mut rng = Rng::new(seed ^ salt.wrapping_mul(0xD6E8_FEB8_6659_FD93));
    rng.next_u64()
}
