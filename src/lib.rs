//! A network conditioner for Bevy apps on aeronet: latency, jitter, loss,
//! bursts, outages and a bandwidth cap, live-tunable and reproducible from a
//! seed, with the failure mode of the real transport underneath.
//!
//! Over a stream (WebSocket, TCP) nothing is ever lost or reordered at the
//! application: a loss stalls the whole direction until the retransmission
//! lands. [`Mode::Stream`] reproduces those stalls as measured against Linux
//! TCP under netem. [`Mode::Datagram`] is UDP-style loss for WebTransport.
//!
//! The model is plain Rust ([`Link`]); the `bevy` feature hooks it under
//! aeronet's transport on every `Session` ([`LinkConditionerPlugin`]), and
//! the `egui` feature adds a live panel ([`LinkConditionerUiPlugin`]).

mod config;
pub mod link;
#[cfg(feature = "bevy")]
mod plugin;
#[cfg(feature = "egui")]
mod ui;

pub use config::{Direction, LinkConditioner, Mode, PRESETS, ParseError};
pub use link::{Link, Stats};
#[cfg(feature = "bevy")]
pub use plugin::{
    ConditionedLink, DefaultLinkConditioner, LinkConditionerPlugin, LinkConditionerSystems,
};
#[cfg(feature = "egui")]
pub use ui::{LinkConditionerUi, LinkConditionerUiPlugin};
