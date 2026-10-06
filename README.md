# bevy_link_conditioner

A network conditioner for Bevy apps on [aeronet] (and so [bevy_replicon]
through `aeronet_replicon`): latency, jitter, loss, loss bursts, outages and a
bandwidth cap, tunable live from an egui panel like Clumsy, reproducible from
a seed, and in the browser too, where netem can't reach.

## What's different

In-app conditioners (lightyear's, replicon's example backend, naia's,
aeronet's old one, netahoy's `--poor-net`) drop and delay packets at random
on receive, with jitter that reorders. Over a WebSocket that is
not what happens. TCP never loses anything at the application: a lost
segment holds back everything behind it until its retransmission lands, then
the backlog arrives at once.

`Mode::Stream` reproduces that, fitted to real Linux TCP under netem with a
game-shaped probe (30 Hz, small messages, Nagle off): a single loss stalls its
direction for `max(next send + RTT, t + 1.25 RTT)`, widened under jitter the
way RACK widens its reorder window; a lost retransmission waits again; with
nothing flowing, a tail-loss probe and then the retransmission timer (SRTT +
200 ms, doubling) take over, and after a timeout the backlog drains in slow
start. A bandwidth cap queues and never drops; the sender drops at the door
past its send buffer (16 KiB, as the aeronet fork's WebSocket server sets it).
Losses on the reverse path never stall the data, so each direction is its own
model.

`Mode::Datagram` is UDP (WebTransport datagrams): losses are gone, jitter can
reorder, duplicates happen, a full bottleneck queue drops the newest. Loss
bursts and outages are timed in real time in both modes, not per packet:
netem's per-packet `gemodel` turns into multi-second stalls once TCP sends
sparsely in timeout backoff.

The hook sits under `aeronet_transport`, on the `Session`: incoming packets
are held between `IoSystems::Poll` and `TransportSystems::Poll`, outgoing ones
between `TransportSystems::Flush` and `IoSystems::Flush`. The transport's
acks, resends and RTT see the conditioned link the way they'd see a real one.
Time is real time, never `Time<Virtual>`.

## Quick start

```rust
use bevy_link_conditioner::{LinkConditionerPlugin, LinkConditionerUiPlugin};

// Conditions every session as `--net=<spec>` or `NET_CONDITION=<spec>` says.
app.add_plugins(LinkConditionerPlugin::from_args());
// The live panel (feature `egui`, needs bevy_egui's EguiPlugin), F2 toggles.
app.add_plugins(LinkConditionerUiPlugin::default());
```

Condition one end of a link only. A client conditions both directions of its
own session (`up` is client to server, `down` server to client); a server
conditions every session through the `DefaultLinkConditioner` resource, or
one session by inserting a `LinkConditioner` component on it.

The model alone needs no Bevy (`default-features = false`):

```rust
let mut link = Link::new(Mode::Stream, Direction::preset("harsh").unwrap(), seed);
link.push(now, packet, len);
while let Some(packet) = link.pop_ready(now) { /* ... */ }
```

## Specs and presets

A spec is a preset and/or `key=value` items, applied left to right; `up.` or
`down.` scopes a key to one direction:

```
harsh
transatlantic,datagram,seed=7
latency=40ms,jitter=8ms,corr=50%,down.loss=2%,burst=80ms,bw=2mbit,outage=400ms/10s
```

Keys: `latency`, `jitter`, `corr` (jitter correlation), `loss`, `burst`
(mean burst length; 0 = independent losses), `bw` (kbit/s), `outage=<hold>/<every>`,
`sndbuf` (stream), `queue`, `reorder`, `dup` (datagram), `mode=stream|datagram`,
`seed`, `off`. The panel prints the spec of what you dialed in, and its netem
equivalent.

| preset        | each way                                        |
|---------------|-------------------------------------------------|
| clean         | nothing                                         |
| lan           | 5 ms                                            |
| transatlantic | 90 ms, 10 ms jitter, 0.3% loss                  |
| mobile        | 60 ms, 20 ms jitter (75% correlated), 1% loss in 80 ms bursts, 400 ms outage every 20 s |
| bursty        | 90 ms, 6% loss in 100 ms bursts                 |
| harsh         | 150 ms, 30 ms jitter, 3% loss                   |
| brutal        | 250 ms, 60 ms jitter, 8% loss, 5% reorder (datagram) |
| stall         | 90 ms, 400 ms outage every 10 s                 |

The names follow the netem profiles of the continuity harness; `bursty` is
its per-packet `gemodel 2% 30%` turned into time.

## Calibration

`tests/calibration.rs` runs the stream model against the measured table (two
minutes of netem per row; model: 16 seeds; netem: the mean of both
directions). Stall: the median stall one loss causes. One-way delay in ms.

| netem (each way)        | stall p50 model / netem | p99 model / netem | max model / netem |
|-------------------------|-------------------------|-------------------|-------------------|
| 25 ms, 2% loss          | 84 / 83                 | 109 / 108         | 175 / 202         |
| 50 ms, 2% loss          | 134 / 133               | 184 / 184         | 317 / 320         |
| 75 ms, 2% loss          | 188 / 192               | 263 / 267         | 450 / 476         |
| 150 ms, 3% loss         | 375 / 379               | 586 / 684         | 901 / 1173        |

| netem (each way)            | p50 model / netem | p90 model / netem | p99 model / netem |
|-----------------------------|-------------------|-------------------|-------------------|
| 90 ms, 10 ms jitter, 0.3%   | 91 / 91           | 105 / 104         | 255 / 211         |
| 75 ms, 20 ms jitter, 2%     | 81 / 85           | 176 / 186         | 333 / 332         |
| 150 ms, 30 ms jitter, 3%    | 175 / 205         | 533 / 500         | 808 / 783         |

Outages (75 ms each way, first delivery after the link comes back, netem
median): 300 ms outage 225 / 419 ms, 1 s outage 542 / 520 ms, 2.5 s outage
442 / 432 ms. A 128 kbit/s cap under too much traffic plateaus at 923-950 ms
(netem 784-826 ms).

## Limits

- Packets come and go at frame boundaries: up to a frame of extra delay on
  release. Incoming packets enter the model when the IO layer received them,
  so slow frames don't bunch up the emulated sender.
- The congestion window is only modelled after a timeout. Real TCP also cuts
  it on every loss, which starts to hold back sends past ~150 ms RTT: there
  the model runs short (RTT 300 ms, 3% loss: p99 -14%, max -23%), and a short
  outage recovers faster than it should (300 ms: 225 vs 419 ms).
- WebTransport over QUIC in a browser has its own congestion control: Chrome
  holds datagrams back when its window is full. Datagram mode doesn't model
  that; a bandwidth cap with a short `queue` is a crude stand-in.
- Conditioning both ends of a link stacks the two.

## Examples

```sh
cargo run --example timeline -- harsh        # headless: what each direction delivers, and when
cargo run --example panel                    # a server and a client in one app, the panel open
cargo run --example panel -- --net=bursty,datagram
```

`panel` joins a server and a client in one app through `aeronet_channel`, so
it runs in the browser with no server; `[package.metadata.wasm-fast]` builds
it with the build box's wasm-fast script.

## Features

- `bevy` (default): the plugin, on `aeronet_io` and `aeronet_transport` 0.21.
- `egui`: the panel, on `bevy_egui` 0.41.

[aeronet]: https://github.com/aecsocket/aeronet
[bevy_replicon]: https://github.com/simgine/bevy_replicon
