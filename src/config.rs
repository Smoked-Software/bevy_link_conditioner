//! What a link does to packets: the knobs, the presets, and the compact text
//! form (`harsh,mode=datagram,down.loss=5%`) the command line and the panel
//! share.

use core::{fmt, str::FromStr, time::Duration};

/// How a link fails: like a TCP byte stream or like UDP datagrams.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "bevy", derive(bevy::reflect::Reflect))]
pub enum Mode {
    /// WebSocket, or anything else over TCP. Nothing is ever lost or
    /// reordered at the application: a lost segment holds back everything
    /// behind it until its retransmission lands, the way Linux TCP does it.
    #[default]
    Stream,
    /// WebTransport datagrams, UDP. Lost packets are gone, jitter can
    /// reorder, and a full bottleneck queue drops the newest packets.
    Datagram,
}

/// One direction of a link. Every field at its default changes nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "bevy", derive(bevy::reflect::Reflect))]
pub struct Direction {
    /// One-way delay added to every packet.
    pub latency: Duration,
    /// Standard deviation of a normally distributed extra delay.
    pub jitter: Duration,
    /// How much each packet's jitter follows the previous one's, in percent.
    pub correlation: f32,
    /// Packets lost on the wire, in percent. A stream retransmits them.
    pub loss: f32,
    /// Mean length of a loss burst. Zero loses packets independently; above
    /// zero the link alternates between clear spells and spells that lose
    /// everything, timed in real time, with `loss` percent of the time bad.
    pub burst: Duration,
    /// Bottleneck rate in kbit/s, 0 for none. Packets queue behind it.
    pub bandwidth: u32,
    /// The link goes dark for `outage_hold` once every `outage_every`.
    pub outage_every: Duration,
    /// How long each outage lasts.
    pub outage_hold: Duration,
    /// Stream only: bytes the sender holds (unsent and unacknowledged)
    /// before it drops new packets at the door, as the aeronet fork's
    /// WebSocket does under a full 16 KiB kernel send buffer.
    pub send_buffer: usize,
    /// Datagram only: the longest a packet may wait behind the bottleneck
    /// before the queue drops it.
    pub queue: Duration,
    /// Datagram only: packets, in percent, sent without the delay, so they
    /// overtake those in flight (netem's `reorder`).
    pub reorder: f32,
    /// Datagram only: packets, in percent, delivered twice.
    pub duplicate: f32,
}

impl Default for Direction {
    fn default() -> Self {
        Self {
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
            correlation: 0.0,
            loss: 0.0,
            burst: Duration::ZERO,
            bandwidth: 0,
            outage_every: Duration::ZERO,
            outage_hold: Duration::ZERO,
            send_buffer: 16 * 1024,
            queue: Duration::from_millis(100),
            reorder: 0.0,
            duplicate: 0.0,
        }
    }
}

/// Both directions of a link, how it fails, and the seed that makes a run
/// repeatable. `up` is client to server, `down` server to client.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "bevy",
    derive(bevy::ecs::component::Component, bevy::reflect::Reflect)
)]
pub struct LinkConditioner {
    /// Off passes every packet straight through and releases held ones.
    pub enabled: bool,
    pub mode: Mode,
    pub up: Direction,
    pub down: Direction,
    pub seed: u64,
}

impl Default for LinkConditioner {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: Mode::Stream,
            up: Direction::default(),
            down: Direction::default(),
            seed: 1,
        }
    }
}

/// The presets, named after the box's netem harness profiles (plus `mobile`),
/// applied the same way in both directions.
pub const PRESETS: [&str; 8] = [
    "clean",
    "lan",
    "transatlantic",
    "mobile",
    "bursty",
    "harsh",
    "brutal",
    "stall",
];

impl Direction {
    /// One of [`PRESETS`].
    pub fn preset(name: &str) -> Option<Self> {
        let ms = Duration::from_millis;
        let base = Self::default();
        Some(match name {
            "clean" => base,
            "lan" => Self { latency: ms(5), ..base },
            "transatlantic" => Self {
                latency: ms(90),
                jitter: ms(10),
                loss: 0.3,
                ..base
            },
            // A phone on the move: wandering delay, losses in clumps, and
            // now and then a handover that drops the link for a moment.
            "mobile" => Self {
                latency: ms(60),
                jitter: ms(20),
                correlation: 75.0,
                loss: 1.0,
                burst: ms(80),
                outage_every: Duration::from_secs(20),
                outage_hold: ms(400),
                ..base
            },
            // The harness's per-packet `gemodel 2% 30%` (runs of ~3 packets,
            // ~6% of them) in real time: per packet it would stretch into
            // multi-second stalls once TCP sends sparsely in RTO backoff.
            "bursty" => Self {
                latency: ms(90),
                loss: 6.0,
                burst: ms(100),
                ..base
            },
            "harsh" => Self {
                latency: ms(150),
                jitter: ms(30),
                loss: 3.0,
                ..base
            },
            "brutal" => Self {
                latency: ms(250),
                jitter: ms(60),
                loss: 8.0,
                reorder: 5.0,
                ..base
            },
            "stall" => Self {
                latency: ms(90),
                outage_every: Duration::from_secs(10),
                outage_hold: ms(400),
                ..base
            },
            _ => return None,
        })
    }

    /// The netem qdisc arguments for this direction (assuming 30 packets a
    /// second where netem counts packets and this counts time), plus what
    /// netem can't express.
    pub fn netem(&self, mode: Mode) -> String {
        let mut args = Vec::new();
        if !self.latency.is_zero() || !self.jitter.is_zero() {
            let mut delay = format!("delay {}", Ms(self.latency));
            if !self.jitter.is_zero() {
                delay += &format!(" {}", Ms(self.jitter));
                if self.correlation > 0.0 {
                    delay += &format!(" {}%", self.correlation);
                }
                delay += " distribution normal";
            }
            args.push(delay);
        }
        if self.loss > 0.0 {
            if self.burst.is_zero() {
                args.push(format!("loss {}%", self.loss));
            } else {
                // Gilbert-Elliott per packet: leave the bad state after
                // `burst` worth of packets, and be there `loss` of the time.
                let run = (self.burst.as_secs_f32() * 30.0).max(1.0);
                let r = 100.0 / run;
                let p = r * self.loss / (100.0 - self.loss).max(0.01);
                args.push(format!("loss gemodel {:.2}% {:.1}%", p, r));
            }
        }
        if mode == Mode::Datagram {
            if self.reorder > 0.0 {
                args.push(format!("reorder {}%", self.reorder));
            }
            if self.duplicate > 0.0 {
                args.push(format!("duplicate {}%", self.duplicate));
            }
        }
        if self.bandwidth > 0 {
            args.push(format!("rate {}kbit", self.bandwidth));
        }
        let mut line = if args.is_empty() {
            "(nothing)".to_owned()
        } else {
            format!("netem {}", args.join(" "))
        };
        if !self.outage_every.is_zero() && !self.outage_hold.is_zero() {
            line += &format!(
                "  # plus loss 100% for {} every {}",
                Ms(self.outage_hold),
                Ms(self.outage_every)
            );
        }
        line
    }
}

impl LinkConditioner {
    /// A preset by name, in stream mode.
    pub fn preset(name: &str) -> Option<Self> {
        let direction = Direction::preset(name)?;
        Some(Self {
            up: direction,
            down: direction,
            ..Self::default()
        })
    }

    /// The direction both ways share, if they do.
    pub fn symmetric(&self) -> Option<&Direction> {
        (self.up == self.down).then_some(&self.up)
    }

    /// The netem equivalent: one qdisc on each side's egress.
    pub fn netem(&self) -> String {
        format!(
            "client egress (up):   tc qdisc add dev eth0 root {}\nserver egress (down): tc qdisc add dev eth0 root {}",
            self.up.netem(self.mode),
            self.down.netem(self.mode)
        )
    }

    /// `--net=<spec>` (or `--net <spec>`) from the command line, else the
    /// `NET_CONDITION` environment variable.
    pub fn from_args() -> Result<Option<Self>, ParseError> {
        let mut args = std::env::args();
        let mut spec = None;
        while let Some(arg) = args.next() {
            if let Some(value) = arg.strip_prefix("--net=") {
                spec = Some(value.to_owned());
            } else if arg == "--net" {
                spec = args.next();
            }
        }
        spec.or_else(|| std::env::var("NET_CONDITION").ok())
            .map(|spec| spec.parse())
            .transpose()
    }
}

/// A spec that didn't parse, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} (expected a preset [{}] and/or key=value pairs: latency jitter corr loss burst bw outage=<hold>/<every> sndbuf queue reorder dup, mode=stream|datagram, seed, off; up./down. for one direction)",
            self.0,
            PRESETS.join(" ")
        )
    }
}

impl std::error::Error for ParseError {}

/// `harsh`, `transatlantic,mode=datagram`, `latency=40ms,down.loss=2%,seed=7`:
/// items apply left to right, a preset replaces both directions.
impl FromStr for LinkConditioner {
    type Err = ParseError;

    fn from_str(spec: &str) -> Result<Self, ParseError> {
        let mut link = Self::default();
        for item in spec.split(',').map(str::trim).filter(|item| !item.is_empty()) {
            let Some((key, value)) = item.split_once('=') else {
                match item {
                    "off" => link.enabled = false,
                    "on" => link.enabled = true,
                    "stream" => link.mode = Mode::Stream,
                    "datagram" => link.mode = Mode::Datagram,
                    _ => {
                        let direction = Direction::preset(item)
                            .ok_or_else(|| ParseError(format!("unknown preset `{item}`")))?;
                        link.up = direction;
                        link.down = direction;
                    }
                }
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            let bad = || ParseError(format!("bad value in `{item}`"));
            match key {
                "mode" => {
                    link.mode = match value {
                        "stream" => Mode::Stream,
                        "datagram" => Mode::Datagram,
                        _ => return Err(bad()),
                    }
                }
                "seed" => link.seed = value.parse().map_err(|_| bad())?,
                _ => {
                    let (directions, key): (&mut [&mut Direction], _) =
                        if let Some(key) = key.strip_prefix("up.") {
                            (&mut [&mut link.up], key)
                        } else if let Some(key) = key.strip_prefix("down.") {
                            (&mut [&mut link.down], key)
                        } else {
                            (&mut [&mut link.up, &mut link.down], key)
                        };
                    for direction in directions {
                        set(direction, key, value).ok_or_else(bad)?;
                    }
                }
            }
        }
        Ok(link)
    }
}

/// Sets one direction's knob from its text form; `None` if the key or the
/// value is unknown.
fn set(direction: &mut Direction, key: &str, value: &str) -> Option<()> {
    match key {
        "latency" | "delay" => direction.latency = duration(value)?,
        "jitter" => direction.jitter = duration(value)?,
        "corr" | "correlation" => direction.correlation = percent(value)?,
        "loss" => direction.loss = percent(value)?,
        "burst" => direction.burst = duration(value)?,
        "bw" | "bandwidth" | "rate" => direction.bandwidth = kbit(value)?,
        "outage" => {
            let (hold, every) = value.split_once('/')?;
            direction.outage_hold = duration(hold)?;
            direction.outage_every = duration(every)?;
        }
        "sndbuf" | "send_buffer" => direction.send_buffer = bytes(value)?,
        "queue" => direction.queue = duration(value)?,
        "reorder" => direction.reorder = percent(value)?,
        "dup" | "duplicate" => direction.duplicate = percent(value)?,
        _ => return None,
    }
    Some(())
}

/// The shortest spec that parses back to this: presets aren't recognized,
/// only the knobs that differ from their defaults are written out.
impl fmt::Display for LinkConditioner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let defaults = Self::default();
        let mut items = Vec::new();
        if !self.enabled {
            items.push("off".to_owned());
        }
        if self.mode == Mode::Datagram {
            items.push("datagram".to_owned());
        }
        if self.seed != defaults.seed {
            items.push(format!("seed={}", self.seed));
        }
        let (up, down) = (knobs(&self.up), knobs(&self.down));
        for ((key, up), (_, down)) in up.into_iter().zip(down) {
            match (up, down) {
                (Some(up), Some(down)) if up == down => items.push(format!("{key}={up}")),
                (up, down) => {
                    items.extend(up.map(|up| format!("up.{key}={up}")));
                    items.extend(down.map(|down| format!("down.{key}={down}")));
                }
            }
        }
        if items.is_empty() {
            items.push("clean".to_owned());
        }
        f.write_str(&items.join(","))
    }
}

/// Each knob's key and, where it differs from the default, its text.
fn knobs(direction: &Direction) -> [(&'static str, Option<String>); 11] {
    let base = Direction::default();
    let text = |differs: bool, text: String| differs.then_some(text);
    let outage = (direction.outage_every, direction.outage_hold);
    [
        ("latency", text(direction.latency != base.latency, Ms(direction.latency).to_string())),
        ("jitter", text(direction.jitter != base.jitter, Ms(direction.jitter).to_string())),
        ("corr", text(direction.correlation != base.correlation, format!("{}%", direction.correlation))),
        ("loss", text(direction.loss != base.loss, format!("{}%", direction.loss))),
        ("burst", text(direction.burst != base.burst, Ms(direction.burst).to_string())),
        ("bw", text(direction.bandwidth != base.bandwidth, format!("{}kbit", direction.bandwidth))),
        (
            "outage",
            text(
                outage != (base.outage_every, base.outage_hold),
                format!("{}/{}", Ms(outage.1), Ms(outage.0)),
            ),
        ),
        ("sndbuf", text(direction.send_buffer != base.send_buffer, direction.send_buffer.to_string())),
        ("queue", text(direction.queue != base.queue, Ms(direction.queue).to_string())),
        ("reorder", text(direction.reorder != base.reorder, format!("{}%", direction.reorder))),
        ("dup", text(direction.duplicate != base.duplicate, format!("{}%", direction.duplicate))),
    ]
}

/// A duration written the way the specs take it: `90ms`, `1.5s`.
struct Ms(Duration);

impl fmt::Display for Ms {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let nanos = self.0.as_nanos();
        if nanos >= 1_000_000_000 && nanos.is_multiple_of(100_000_000) {
            write!(f, "{}s", self.0.as_secs_f64())
        } else {
            write!(f, "{}ms", nanos as f64 / 1e6)
        }
    }
}

/// `90ms`, `1.5s`, or a bare number of milliseconds.
fn duration(text: &str) -> Option<Duration> {
    let (number, scale) = if let Some(ms) = text.strip_suffix("ms") {
        (ms, 1e6)
    } else if let Some(s) = text.strip_suffix('s') {
        (s, 1e9)
    } else {
        (text, 1e6)
    };
    let value: f64 = number.trim().parse().ok()?;
    (value >= 0.0).then(|| Duration::from_nanos((value * scale).round() as u64))
}

/// `2.5%` or a bare `2.5`, in percent, 0 to 100.
fn percent(text: &str) -> Option<f32> {
    let value: f32 = text.strip_suffix('%').unwrap_or(text).trim().parse().ok()?;
    (0.0..=100.0).contains(&value).then_some(value)
}

/// `128kbit`, `2mbit`, or a bare number of kbit/s.
fn kbit(text: &str) -> Option<u32> {
    let text = text.to_ascii_lowercase();
    let (number, scale) = if let Some(m) = text.strip_suffix("mbit") {
        (m, 1000.0)
    } else {
        (text.strip_suffix("kbit").unwrap_or(&text), 1.0)
    };
    let value: f64 = number.trim().parse().ok()?;
    (value >= 0.0).then(|| (value * scale).round() as u32)
}

/// `16k`, `16KiB`, or a bare number of bytes.
fn bytes(text: &str) -> Option<usize> {
    let text = text.to_ascii_lowercase();
    let number = text.strip_suffix("kib").or_else(|| text.strip_suffix('k'));
    match number {
        Some(k) => k.trim().parse::<f64>().ok().map(|k| (k * 1024.0).round() as usize),
        None => text.trim().parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_parse_and_print_back() {
        for name in PRESETS {
            let link: LinkConditioner = name.parse().unwrap();
            assert_eq!(link, LinkConditioner::preset(name).unwrap());
            assert_eq!(link.to_string().parse::<LinkConditioner>().unwrap(), link, "{name}");
        }
    }

    #[test]
    fn spec_round_trips() {
        let spec = "harsh,datagram,seed=7,down.loss=5%,up.bw=128kbit,outage=400ms/10s,sndbuf=8k,reorder=2.5%";
        let link: LinkConditioner = spec.parse().unwrap();
        assert_eq!(link.mode, Mode::Datagram);
        assert_eq!(link.seed, 7);
        assert_eq!(link.down.loss, 5.0);
        assert_eq!(link.up.loss, 3.0);
        assert_eq!(link.up.bandwidth, 128);
        assert_eq!(link.down.bandwidth, 0);
        assert_eq!(link.up.outage_hold, Duration::from_millis(400));
        assert_eq!(link.up.outage_every, Duration::from_secs(10));
        assert_eq!(link.up.send_buffer, 8192);
        assert_eq!(link.to_string().parse::<LinkConditioner>().unwrap(), link);
        assert_eq!(LinkConditioner::default().to_string(), "clean");
    }

    #[test]
    fn bad_specs_say_why() {
        assert!("nowhere".parse::<LinkConditioner>().unwrap_err().0.contains("nowhere"));
        assert!("loss=200%".parse::<LinkConditioner>().is_err());
        assert!("latency=-5ms".parse::<LinkConditioner>().is_err());
        assert!("sideways.loss=1%".parse::<LinkConditioner>().is_err());
    }
}
