//! The stream model against real Linux TCP under netem.
//!
//! The fixture is the measured table: a game-shaped probe (400 B down, 40 B
//! up, both at 30 Hz, Nagle off, 16 KiB send buffer) over a veth pair with
//! the same netem delay and loss each way, two minutes per row. Per row: the
//! median stall a single loss causes (the onset of each head-of-line block
//! longer than 0.8 RTT), and the p99 and max of the one-way delay, as the
//! mean of the two directions (each is one two-minute sample of the same
//! process; at RTT 300 their p99s differ by 20%).
//!
//! At RTT 300 the model runs short of real TCP: past ~150 ms the congestion
//! window, cut on each loss and not modelled, starts to hold back sends.

use core::time::Duration;

use bevy_link_conditioner::{Direction, Link, Mode};

struct Measured {
    name: &'static str,
    one_way_ms: u64,
    loss: f32,
    stall_p50: f64,
    p99: f64,
    max: f64,
}

/// `(down + up) / 2` of `analyze`/`onset` on `~/scratch/tcp-netem/out/<name>`.
const MEASURED: [Measured; 4] = [
    Measured {
        name: "rtt50_l2",
        one_way_ms: 25,
        loss: 2.0,
        stall_p50: 83.0,
        p99: 108.5,
        max: 201.5,
    },
    Measured {
        name: "rtt100_l2",
        one_way_ms: 50,
        loss: 2.0,
        stall_p50: 133.0,
        p99: 184.0,
        max: 320.0,
    },
    Measured {
        name: "rtt150_l2",
        one_way_ms: 75,
        loss: 2.0,
        stall_p50: 192.0,
        p99: 267.0,
        max: 476.0,
    },
    Measured {
        name: "rtt300_l3",
        one_way_ms: 150,
        loss: 3.0,
        stall_p50: 379.0,
        p99: 684.0,
        max: 1173.0,
    },
];

const HZ: f64 = 30.0;
const SECONDS: f64 = 120.0;
const SEEDS: u64 = 16;

/// One-way delays (ms) of a 30 Hz, 400 B stream through `link`, popped
/// every millisecond.
fn one_way_delays(link: &mut Link<u64>) -> Vec<f64> {
    let count = (SECONDS * HZ) as u64;
    let mut sent_at = Vec::new();
    let mut delays = Vec::new();
    let mut next = 0;
    let mut ms = 0;
    while delays.len() < count as usize && ms < (SECONDS as u64 + 60) * 1000 {
        let now = Duration::from_millis(ms);
        while next < count && Duration::from_secs_f64(next as f64 / HZ) <= now {
            let at = Duration::from_secs_f64(next as f64 / HZ);
            sent_at.push(at);
            link.push(at, next, 400);
            next += 1;
        }
        while let Some(seq) = link.pop_ready(now) {
            delays.push((now - sent_at[seq as usize]).as_secs_f64() * 1000.0);
        }
        ms += 1;
    }
    delays
}

fn percentile(values: &[f64], q: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((q * sorted.len() as f64) as usize).min(sorted.len() - 1)]
}

/// Stalls a single loss starts: the analysis script's onsets above 0.8 RTT.
fn stall_onsets(delays: &[f64], rtt: f64) -> Vec<f64> {
    let base = delays.iter().copied().fold(f64::INFINITY, f64::min);
    let extra: Vec<f64> = delays.iter().map(|delay| delay - base).collect();
    extra
        .windows(2)
        .filter(|pair| pair[1] > pair[0] - 1000.0 / HZ + 15.0 && pair[1] > 15.0)
        .map(|pair| pair[1])
        .filter(|&stall| stall > 0.8 * rtt)
        .collect()
}

struct Modelled {
    stall_p50: f64,
    p99: f64,
    max: f64,
}

fn model(row: &Measured) -> Modelled {
    let direction = Direction {
        latency: Duration::from_millis(row.one_way_ms),
        loss: row.loss,
        ..Direction::default()
    };
    let (mut stalls, mut p99s, mut maxes) = (Vec::new(), Vec::new(), Vec::new());
    for seed in 0..SEEDS {
        let mut link = Link::new(Mode::Stream, direction, seed);
        let delays = one_way_delays(&mut link);
        assert_eq!(
            delays.len(),
            (SECONDS * HZ) as usize,
            "a stream delivers everything"
        );
        stalls.extend(stall_onsets(&delays, 2.0 * row.one_way_ms as f64));
        p99s.push(percentile(&delays, 0.99));
        maxes.push(delays.iter().copied().fold(0.0, f64::max));
    }
    Modelled {
        stall_p50: percentile(&stalls, 0.5),
        p99: p99s.iter().sum::<f64>() / p99s.len() as f64,
        max: percentile(&maxes, 0.5),
    }
}

#[test]
fn stream_matches_measured_tcp() {
    let mut failures = Vec::new();
    for row in &MEASURED {
        let modelled = model(row);
        println!(
            "{:10} stall p50 {:5.0} (netem {:5.0})  p99 {:5.0} (netem {:5.0})  max {:5.0} (netem {:5.0})",
            row.name,
            modelled.stall_p50,
            row.stall_p50,
            modelled.p99,
            row.p99,
            modelled.max,
            row.max
        );
        for (what, got, want, tolerance) in [
            ("stall p50", modelled.stall_p50, row.stall_p50, 0.10),
            ("p99", modelled.p99, row.p99, 0.15),
            ("max", modelled.max, row.max, 0.25),
        ] {
            if (got / want - 1.0).abs() > tolerance {
                failures.push(format!(
                    "{} {what}: {got:.0} ms vs measured {want:.0} ms",
                    row.name
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Jitter and loss together: netem's jitter reorders on the wire, TCP puts
/// it back in order, which the model does directly. Mean of both directions.
#[test]
fn jittery_profiles_track_measured_tcp() {
    // (name, one-way ms, jitter ms, loss %, measured p50, p90, p99)
    let rows = [
        ("transatlantic", 90, 10, 0.3, 91.0, 104.0, 211.0),
        ("rtt150_j20_l2", 75, 20, 2.0, 85.0, 186.5, 331.5),
        ("harsh", 150, 30, 3.0, 205.0, 500.5, 783.0),
    ];
    let mut failures = Vec::new();
    for (name, one_way, jitter, loss, p50, p90, p99) in rows {
        let direction = Direction {
            latency: Duration::from_millis(one_way),
            jitter: Duration::from_millis(jitter),
            loss,
            ..Direction::default()
        };
        let mut all = Vec::new();
        for seed in 0..SEEDS {
            all.extend(one_way_delays(&mut Link::new(
                Mode::Stream,
                direction,
                seed,
            )));
        }
        let got = [0.5, 0.9, 0.99].map(|q| percentile(&all, q));
        println!(
            "{name:14} p50 {:4.0} (netem {p50:4.0})  p90 {:4.0} (netem {p90:4.0})  p99 {:4.0} (netem {p99:4.0})",
            got[0], got[1], got[2]
        );
        for (got, want) in got.into_iter().zip([p50, p90, p99]) {
            if (got / want - 1.0).abs() > 0.3 {
                failures.push(format!("{name}: {got:.0} ms vs measured {want:.0} ms"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
