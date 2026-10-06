//! The link model's promises, in simulated time.

use core::time::Duration;

use bevy_link_conditioner::{Direction, Link, LinkConditioner, Mode};

const MS: Duration = Duration::from_millis(1);

/// Sends `count` packets of `len` bytes at `hz`, pops every millisecond,
/// and returns `(seq, sent, popped)` in delivery order.
fn run(link: &mut Link<u64>, hz: f64, len: usize, count: u64) -> Vec<(u64, Duration, Duration)> {
    let sent_at = |seq: u64| Duration::from_secs_f64(seq as f64 / hz);
    let end = sent_at(count) + Duration::from_secs(120);
    let (mut next, mut now, mut out) = (0, Duration::ZERO, Vec::new());
    while now < end {
        while next < count && sent_at(next) <= now {
            link.push(sent_at(next), next, len);
            next += 1;
        }
        while let Some(seq) = link.pop_ready(now) {
            out.push((seq, sent_at(seq), now));
        }
        now += MS;
    }
    out
}

fn everything_bad() -> Direction {
    Direction {
        jitter: Duration::from_millis(40),
        loss: 5.0,
        burst: Duration::from_millis(60),
        bandwidth: 2000,
        outage_every: Duration::from_secs(7),
        outage_hold: Duration::from_millis(1500),
        ..Direction::preset("harsh").unwrap()
    }
}

#[test]
fn stream_delivers_everything_in_order() {
    let direction = Direction {
        send_buffer: 1 << 20,
        ..everything_bad()
    };
    let mut link = Link::new(Mode::Stream, direction, 3);
    let delivered = run(&mut link, 30.0, 400, 3000);
    let stats = link.stats();
    assert!(stats.lost > 100 && stats.timeouts > 0, "{stats:?}");
    assert_eq!(stats.dropped, 0, "under the send buffer nothing is dropped");
    let seqs: Vec<u64> = delivered.iter().map(|&(seq, ..)| seq).collect();
    assert_eq!(seqs, (0..3000).collect::<Vec<_>>());
}

#[test]
fn stream_drops_only_at_the_send_door() {
    let direction = Direction {
        send_buffer: 4096,
        ..everything_bad()
    };
    let mut link = Link::new(Mode::Stream, direction, 3);
    let delivered = run(&mut link, 60.0, 400, 6000);
    let stats = link.stats().clone();
    assert!(stats.dropped > 0, "{stats:?}");
    assert_eq!(delivered.len() as u64 + stats.dropped, 6000);
    assert!(delivered.windows(2).all(|pair| pair[0].0 < pair[1].0), "in order");
}

#[test]
fn stream_bandwidth_queues_up_to_the_send_buffer() {
    // 600 B at 30 Hz into 128 kbit/s: more than fits. The delay climbs until
    // the send buffer is full, then holds; the excess is dropped at the door.
    let direction = Direction {
        latency: Duration::from_millis(50),
        bandwidth: 128,
        ..Direction::default()
    };
    let mut link = Link::new(Mode::Stream, direction, 1);
    let delivered = run(&mut link, 30.0, 600, 3600);
    let late: Vec<f64> = delivered[1800..].iter().map(|&(_, sent, at)| (at - sent).as_secs_f64()).collect();
    let (low, high) = late.iter().fold((f64::MAX, 0.0_f64), |(low, high), &d| (low.min(d), high.max(d)));
    println!("plateau {:.0}..{:.0} ms (netem rate128_over: 784..826 ms)", low * 1000.0, high * 1000.0);
    assert!(low > 0.6 && high < 1.2, "plateau {low}..{high} s");
    assert!(link.stats().dropped > 0);
}

#[test]
fn stream_outage_recovers_on_the_backoff_clock() {
    // RTT 150 ms, both ways dark every 10 s: nothing arrives meanwhile, and
    // delivery resumes at the first probe or timeout after the link is back.
    // Measured: median first delivery after the link returns (both
    // directions of `~/scratch/tcp-netem/out/out<hold>`).
    for (hold, measured) in [(300, 419), (1000, 520), (2500, 432)] {
        let direction = Direction {
            latency: Duration::from_millis(75),
            outage_every: Duration::from_secs(10),
            outage_hold: Duration::from_millis(hold),
            // The probe blocked rather than dropped.
            send_buffer: 1 << 20,
            ..Direction::default()
        };
        let mut link = Link::new(Mode::Stream, direction, 1);
        let delivered = run(&mut link, 30.0, 400, 30 * 60);
        assert_eq!(delivered.len(), 30 * 60);
        let resumed: Vec<u128> = (1..6)
            .map(|outage| {
                let up = Duration::from_millis(10_000 * outage + hold);
                let first = delivered.iter().map(|&(.., at)| at).find(|&at| at > up).unwrap();
                (first - up).as_millis()
            })
            .collect();
        println!("{hold} ms outage: first delivery {resumed:?} ms after the link returns (netem median {measured})");
        assert!(
            resumed.iter().all(|&ms| ms * 2 > measured && ms < measured * 3 / 2),
            "{resumed:?}"
        );
    }
}

#[test]
fn datagram_loss_and_duplicates_match_the_settings() {
    let direction = Direction {
        latency: Duration::from_millis(30),
        loss: 5.0,
        duplicate: 2.0,
        ..Direction::default()
    };
    let mut link = Link::new(Mode::Datagram, direction, 9);
    let count = 100_000;
    let delivered = run(&mut link, 1000.0, 100, count);
    let stats = link.stats();
    let lost = stats.lost as f64 / count as f64;
    let duplicated = stats.duplicated as f64 / (count - stats.lost) as f64;
    assert!((lost - 0.05).abs() < 0.003, "lost {lost}");
    assert!((duplicated - 0.02).abs() < 0.002, "duplicated {duplicated}");
    assert_eq!(delivered.len() as u64, count - stats.lost + stats.duplicated);
}

#[test]
fn datagram_bursts_lose_the_right_share_in_runs() {
    let direction = Direction {
        loss: 6.0,
        burst: Duration::from_millis(100),
        ..Direction::default()
    };
    let mut link = Link::new(Mode::Datagram, direction, 5);
    let count = 30 * 1200;
    let delivered = run(&mut link, 30.0, 100, count);
    let lost = 1.0 - delivered.len() as f64 / count as f64;
    // Runs of consecutive losses: ~100 ms at 30 Hz is ~3 packets.
    let gaps: Vec<u64> = delivered
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0 - 1)
        .filter(|&gap| gap > 0)
        .collect();
    let mean_run = gaps.iter().sum::<u64>() as f64 / gaps.len() as f64;
    println!("lost {:.1}%, mean run {mean_run:.1} packets", lost * 100.0);
    assert!((lost - 0.06).abs() < 0.015, "lost {lost}");
    assert!((2.0..5.0).contains(&mean_run), "mean run {mean_run}");
}

#[test]
fn datagram_queue_drops_the_excess() {
    let direction = Direction {
        bandwidth: 256,
        queue: Duration::from_millis(100),
        ..Direction::default()
    };
    let mut link = Link::new(Mode::Datagram, direction, 1);
    let delivered = run(&mut link, 60.0, 1000, 3600);
    let worst = delivered.iter().map(|&(_, sent, at)| at - sent).max().unwrap();
    assert!(link.stats().dropped > 1000);
    assert!(worst < Duration::from_millis(150), "queue bounded: {worst:?}");
}

#[test]
fn same_seed_same_timeline() {
    let timeline = |mode, seed| {
        let mut link = Link::new(mode, everything_bad(), seed);
        run(&mut link, 60.0, 300, 3000)
    };
    for mode in [Mode::Stream, Mode::Datagram] {
        assert_eq!(timeline(mode, 11), timeline(mode, 11));
        assert_ne!(timeline(mode, 11), timeline(mode, 12));
    }
}

#[test]
fn a_change_of_mode_releases_what_is_held() {
    let mut link = Link::new(Mode::Stream, Direction::preset("harsh").unwrap(), 1);
    for seq in 0..10 {
        link.push(Duration::from_millis(seq * 10), seq, 100);
    }
    link.set_config(Mode::Datagram, Direction::default());
    let released: Vec<u64> = core::iter::from_fn(|| link.pop_ready(Duration::from_millis(100))).collect();
    assert_eq!(released, (0..10).collect::<Vec<_>>());
}

#[test]
fn presets_condition_something() {
    for name in bevy_link_conditioner::PRESETS.into_iter().skip(1) {
        let conditioner = LinkConditioner::preset(name).unwrap();
        let mut link = Link::new(conditioner.mode, conditioner.down, 1);
        let delivered = run(&mut link, 30.0, 400, 30 * 30);
        let worst = delivered.iter().map(|&(_, sent, at)| at - sent).max().unwrap();
        assert!(worst >= Duration::from_millis(5), "{name}");
    }
}
