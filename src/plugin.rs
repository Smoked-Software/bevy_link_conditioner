//! The Bevy side: a [`Link`] each way on every conditioned aeronet
//! `Session`, below `aeronet_transport`, so its acks, resends and RTT see
//! the conditioned link the way they would see a real one.
//!
//! Incoming packets are taken out of `Session::recv` after the IO layer
//! polls and before the transport reads them; outgoing ones out of
//! `Session::send` after the transport flushes and before the IO layer
//! does. Held packets live in [`ConditionedLink`] until they are due, so
//! aeronet's own buffer clearing never sees them. Time is real time, never
//! `Time<Virtual>`: a slowed-down game still has a real-time network.

use core::time::Duration;

use aeronet_io::{IoSystems, Session, bytes::Bytes, packet::RecvPacket, server::Server};
use aeronet_transport::{Transport, TransportSystems};
use bevy::{platform::time::Instant, prelude::*};

use crate::{Link, LinkConditioner, link::mix_seed};

/// Conditions sessions: each one with a [`LinkConditioner`] component, and
/// every new one while a [`DefaultLinkConditioner`] exists.
///
/// Condition one side of a link only: a client conditions both directions
/// of its own session, a server every session (or the ones you choose).
#[derive(Default)]
pub struct LinkConditionerPlugin {
    /// Inserted as the [`DefaultLinkConditioner`].
    pub default: Option<LinkConditioner>,
}

impl LinkConditionerPlugin {
    /// Conditions every session as `--net=<spec>` or `NET_CONDITION` says,
    /// and none if neither is given.
    ///
    /// # Panics
    ///
    /// On a spec that doesn't parse, saying why.
    pub fn from_args() -> Self {
        match LinkConditioner::from_args() {
            Ok(default) => Self { default },
            Err(err) => panic!("--net: {err}"),
        }
    }
}

/// Copied onto every session that connects, and kept in step with it.
#[derive(Resource, Clone, Debug, Default, PartialEq, Deref, DerefMut)]
pub struct DefaultLinkConditioner(pub LinkConditioner);

/// The session's [`LinkConditioner`] is the default's copy.
#[derive(Component)]
struct FollowsDefault;

/// The links a session's packets go through, and what they hold.
#[derive(Component)]
pub struct ConditionedLink {
    epoch: Instant,
    seed: u64,
    server_side: bool,
    up: Link<Bytes>,
    down: Link<Bytes>,
}

impl ConditionedLink {
    /// Client to server.
    pub fn up(&self) -> &Link<Bytes> {
        &self.up
    }

    /// Server to client.
    pub fn down(&self) -> &Link<Bytes> {
        &self.down
    }

    /// The links' clock: real time since they were made.
    pub fn now(&self) -> Duration {
        Instant::now().saturating_duration_since(self.epoch)
    }

    fn new(entity: Entity, server_side: bool, config: &LinkConditioner) -> Self {
        // Both ways, and every session, get their own random numbers.
        let salt = u64::from(entity.index_u32()) << 1;
        Self {
            epoch: Instant::now(),
            seed: config.seed,
            server_side,
            up: Link::new(config.mode, config.up, mix_seed(config.seed, salt)),
            down: Link::new(config.mode, config.down, mix_seed(config.seed, salt | 1)),
        }
    }

    /// `(incoming, outgoing)` for this end of the link.
    fn ways(&mut self) -> (&mut Link<Bytes>, &mut Link<Bytes>) {
        if self.server_side {
            (&mut self.up, &mut self.down)
        } else {
            (&mut self.down, &mut self.up)
        }
    }
}

#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkConditionerSystems {
    /// Between `IoSystems::Poll` and `TransportSystems::Poll`.
    Incoming,
    /// Between `TransportSystems::Flush` and `IoSystems::Flush`.
    Outgoing,
}

impl Plugin for LinkConditionerPlugin {
    fn build(&self, app: &mut App) {
        if let Some(default) = &self.default {
            info!("conditioning every session: {default}");
            app.insert_resource(DefaultLinkConditioner(default.clone()));
        }
        app.configure_sets(
            PreUpdate,
            LinkConditionerSystems::Incoming
                .after(IoSystems::Poll)
                .before(TransportSystems::Poll),
        )
        .configure_sets(
            PostUpdate,
            LinkConditionerSystems::Outgoing
                .after(TransportSystems::Flush)
                .before(IoSystems::Flush),
        )
        .add_systems(
            PreUpdate,
            (follow_default, configure, condition_incoming)
                .chain()
                .in_set(LinkConditionerSystems::Incoming),
        )
        .add_systems(
            PostUpdate,
            condition_outgoing.in_set(LinkConditionerSystems::Outgoing),
        )
        .add_observer(adopt_default);
    }
}

fn adopt_default(
    session: On<Add, Session>,
    default: Option<Res<DefaultLinkConditioner>>,
    mut commands: Commands,
) {
    if let Some(default) = default {
        commands
            .entity(session.event_target())
            .insert((default.0.clone(), FollowsDefault));
    }
}

fn follow_default(
    default: Option<Res<DefaultLinkConditioner>>,
    mut sessions: Query<&mut LinkConditioner, With<FollowsDefault>>,
) {
    let Some(default) = default.filter(|default| default.is_changed()) else {
        return;
    };
    for mut conditioner in &mut sessions {
        conditioner.set_if_neq(default.0.clone());
    }
}

/// Makes, retunes, reseeds or lets go of a session's links as its
/// [`LinkConditioner`] says.
fn configure(
    mut sessions: Query<
        (
            Entity,
            &LinkConditioner,
            Option<&mut ConditionedLink>,
            &mut Session,
            Option<&ChildOf>,
        ),
        Changed<LinkConditioner>,
    >,
    servers: Query<(), With<Server>>,
    mut commands: Commands,
) {
    for (entity, config, link, mut session, child_of) in &mut sessions {
        let server_side = child_of.is_some_and(|child_of| servers.contains(child_of.parent()));
        match link {
            Some(mut link) if config.enabled && config.seed == link.seed => {
                link.up.set_config(config.mode, config.up);
                link.down.set_config(config.mode, config.down);
            }
            link => {
                if let Some(mut link) = link {
                    // Off, or a new seed: whatever is held goes now.
                    let (incoming, outgoing) = link.ways();
                    let now = Instant::now();
                    session.recv.extend(incoming.drain().map(|payload| RecvPacket { recv_at: now, payload }));
                    session.send.extend(outgoing.drain());
                    commands.entity(entity).remove::<ConditionedLink>();
                }
                if config.enabled {
                    commands
                        .entity(entity)
                        .insert(ConditionedLink::new(entity, server_side, config));
                }
            }
        }
    }
}

fn condition_incoming(mut sessions: Query<(&mut Session, &mut ConditionedLink)>) {
    for (mut session, mut link) in &mut sessions {
        let now = Instant::now();
        let at = now.saturating_duration_since(link.epoch);
        let epoch = link.epoch;
        let (incoming, _) = link.ways();
        for packet in core::mem::take(&mut session.recv) {
            // Sent when the IO layer got it, not at the next frame: a slow
            // frame mustn't bunch up the emulated sender.
            let sent = packet.recv_at.saturating_duration_since(epoch);
            let len = packet.payload.len();
            incoming.push(sent, packet.payload, len);
        }
        while let Some(payload) = incoming.pop_ready(at) {
            session.recv.push(RecvPacket { recv_at: now, payload });
        }
    }
}

fn condition_outgoing(
    mut sessions: Query<(
        &mut Session,
        &mut ConditionedLink,
        &LinkConditioner,
        Option<&Transport>,
    )>,
) {
    for (mut session, mut link, config, transport) in &mut sessions {
        // The emulated round trip: ours on top of the real link's, which the
        // transport's shortest RTT sample includes.
        let ours = config.up.latency + config.down.latency;
        let rtt = transport.map_or(ours, |transport| transport.rtt().min().max(ours));
        link.up.set_round_trip(rtt);
        link.down.set_round_trip(rtt);

        let at = Instant::now().saturating_duration_since(link.epoch);
        let (_, outgoing) = link.ways();
        for payload in core::mem::take(&mut session.send) {
            let len = payload.len();
            outgoing.push(at, payload, len);
        }
        while let Some(payload) = outgoing.pop_ready(at) {
            session.send.push(payload);
        }
    }
}
