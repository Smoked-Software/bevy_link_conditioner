//! Game-shaped traffic through a conditioned link, in simulated time: what
//! each direction delivers, how late, and how it stalls.
//!
//! ```sh
//! cargo run --example timeline -- harsh
//! cargo run --example timeline -- "transatlantic,datagram" 120
//! ```

use core::time::Duration;

use bevy_link_conditioner::{Link, LinkConditioner, Mode};

fn main() {
    let mut args = std::env::args().skip(1);
    let spec = args.next().unwrap_or_else(|| "harsh".to_owned());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let conditioner: LinkConditioner = spec.parse().unwrap_or_else(|err| panic!("{err}"));
    let rtt = conditioner.up.latency + conditioner.down.latency;

    println!(
        "{conditioner} ({:?}), {seconds} s\n{}\n",
        conditioner.mode,
        conditioner.netem()
    );
    for (name, direction, hz, len, salt) in [
        ("up   (60 Hz x 60 B commands) ", conditioner.up, 60, 60, 1),
        (
            "down (30 Hz x 400 B snapshots)",
            conditioner.down,
            30,
            400,
            2,
        ),
    ] {
        let mut link = Link::new(conditioner.mode, direction, conditioner.seed ^ salt);
        link.set_round_trip(rtt);
        let delays = simulate(&mut link, hz, len, seconds);
        report(name, &link, &delays, direction.latency, seconds);
    }
}

/// `(sent, one-way delay)` of every packet delivered, popping each millisecond.
fn simulate(
    link: &mut Link<Duration>,
    hz: u64,
    len: usize,
    seconds: u64,
) -> Vec<(Duration, Duration)> {
    let count = hz * seconds;
    let sent_at = |seq: u64| Duration::from_secs_f64(seq as f64 / hz as f64);
    let (mut next, mut delays) = (0, Vec::new());
    for ms in 0..(seconds + 30) * 1000 {
        let now = Duration::from_millis(ms);
        while next < count && sent_at(next) <= now {
            link.push(sent_at(next), sent_at(next), len);
            next += 1;
        }
        while let Some(sent) = link.pop_ready(now) {
            delays.push((sent, now - sent));
        }
    }
    delays
}

fn report(
    name: &str,
    link: &Link<Duration>,
    delays: &[(Duration, Duration)],
    latency: Duration,
    seconds: u64,
) {
    let stats = link.stats();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let mut sorted: Vec<f64> = delays.iter().map(|&(_, delay)| ms(delay)).collect();
    sorted.sort_by(f64::total_cmp);
    let at = |q: f64| {
        sorted.get(((q * sorted.len() as f64) as usize).min(sorted.len().saturating_sub(1)))
    };
    // Stalls: runs of packets held more than 15 ms past the latency.
    let (mut stalls, mut stalled) = (0, 0);
    let mut in_stall = false;
    for &(_, delay) in delays {
        let late = ms(delay) - ms(latency) > 15.0;
        stalls += (late && !in_stall) as u32;
        stalled += late as u32;
        in_stall = late;
    }
    println!("{name}");
    println!(
        "  sent {}  delivered {}  lost on wire {}  retransmits {} ({} on timeout)  dropped {}  duplicated {}",
        stats.sent,
        stats.delivered,
        stats.lost,
        stats.retransmits,
        stats.timeouts,
        stats.dropped,
        stats.duplicated
    );
    if let (Some(p50), Some(p90), Some(p99), Some(max)) =
        (at(0.5), at(0.9), at(0.99), sorted.last())
    {
        println!("  one-way ms  p50 {p50:.0}  p90 {p90:.0}  p99 {p99:.0}  max {max:.0}");
    }
    println!(
        "  stalls {stalls} ({:.1}/min), {:.1}% of packets held >15 ms",
        stalls as f64 * 60.0 / seconds as f64,
        100.0 * stalled as f64 / delays.len().max(1) as f64
    );
    if link.mode() == Mode::Stream {
        let in_order = delays.windows(2).all(|pair| pair[0].0 < pair[1].0);
        println!("  in order: {in_order}");
    }
    println!(
        "  worst delay per second: {}\n",
        strip(delays, seconds.min(80))
    );
}

/// The worst delay of each second, as a row of bars.
fn strip(delays: &[(Duration, Duration)], seconds: u64) -> String {
    let mut worst = vec![0.0_f64; seconds as usize];
    for &(sent, delay) in delays {
        if let Some(slot) = worst.get_mut(sent.as_secs() as usize) {
            *slot = slot.max(delay.as_secs_f64());
        }
    }
    let top = worst.iter().copied().fold(1e-3, f64::max);
    let bars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let text: String = worst
        .iter()
        .map(|w| bars[((w / top) * 7.0).round() as usize])
        .collect();
    format!("{text} (█ = {:.0} ms)", top * 1000.0)
}
