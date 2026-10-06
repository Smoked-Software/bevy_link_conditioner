//! A server and a client in one app, joined by aeronet's in-memory channel,
//! with the client's end conditioned. Left, the server's world: the truth.
//! Right, the client's replicated copy, as it arrives through the link, the
//! truth outlined behind it. Arrow keys or WASD steer the white square, so
//! the round trip shows too. F2 shows and hides the panel.
//!
//! ```sh
//! cargo run --example panel
//! cargo run --example panel -- --net=bursty,datagram
//! ```
//!
//! It runs in the browser as well: nothing leaves the page.

use aeronet_channel::{ChannelIo, ChannelIoPlugin};
use aeronet_io::{IoSystems, server::Server};
use aeronet_replicon::{
    client::{AeronetRepliconClient, AeronetRepliconClientPlugin},
    server::{AeronetRepliconServer, AeronetRepliconServerPlugin},
};
use bevy::{
    camera::ScalingMode, platform::time::Instant, prelude::*, state::app::StatesPlugin,
};
use bevy_egui::EguiPlugin;
use bevy_link_conditioner::{LinkConditioner, LinkConditionerPlugin, LinkConditionerUiPlugin};
use bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};

/// Half the side of each pane's arena.
const ARENA: f32 = 170.0;
/// Where the two panes sit.
const SERVER_PANE: Vec2 = Vec2::new(-200.0, -10.0);
const CLIENT_PANE: Vec2 = Vec2::new(200.0, -10.0);
const SQUARE: f32 = 22.0;
const PLAYER_SPEED: f32 = 220.0;

/// A square the server moves and replicates: 0 is the player's.
#[derive(Component, Serialize, Deserialize, Clone, Copy)]
struct Square {
    id: u8,
    position: Vec2,
}

/// Which way the player is pushing, sent every frame.
#[derive(Message, Serialize, Deserialize, Clone, Copy, Default)]
struct Steer(Vec2);

/// The server, a whole second app, stepped once per client frame.
struct ServerApp(App);

/// The server's squares this frame, for drawing the truth.
#[derive(Resource, Default)]
struct Truth(Vec<Square>);

fn main() -> AppExit {
    let link = LinkConditionerPlugin::from_args();
    let link = LinkConditionerPlugin {
        default: link.default.or_else(|| LinkConditioner::preset("harsh")),
    };
    App::new()
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    title: "bevy_link_conditioner".into(),
                    canvas: Some("#bevy".into()),
                    fit_canvas_to_parent: true,
                    ..default()
                }),
                ..default()
            }),
            RepliconPlugins,
            AeronetRepliconClientPlugin,
            ChannelIoPlugin,
            EguiPlugin::default(),
            link,
            LinkConditionerUiPlugin {
                open: true,
                ..default()
            },
            protocol,
        ))
        .insert_non_send(ServerApp(server_app()))
        .init_resource::<Truth>()
        .add_systems(Startup, (connect, scene))
        .add_systems(PreUpdate, step_server.before(IoSystems::Poll))
        .add_systems(
            Update,
            (
                steer.run_if(in_state(ClientState::Connected)),
                show_truth,
                show_replicated,
                draw,
            ),
        )
        .add_observer(dress_replicated)
        .run()
}

/// What both apps agree on.
fn protocol(app: &mut App) {
    app.replicate::<Square>()
        .add_client_message::<Steer>(Channel::Unreliable);
}

fn server_app() -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        StatesPlugin,
        RepliconPlugins,
        AeronetRepliconServerPlugin,
        ChannelIoPlugin,
        protocol,
    ))
    // Replicated 30 times a second, a typical game tick.
    .insert_resource(Time::<Fixed>::from_hz(30.0))
    .init_resource::<PlayerSteer>()
    .add_systems(Startup, spawn_squares)
    .add_systems(Update, read_steer)
    .add_systems(FixedUpdate, move_squares);
    app.finish();
    app.cleanup();
    app
}

/// Opens the channel: one end the client, the other a session on the server.
fn connect(world: &mut World) {
    let (client, server) = ChannelIo::new();
    world.spawn((Name::new("client"), AeronetRepliconClient, client));
    let mut server_app = world.non_send_mut::<ServerApp>();
    let server_world = server_app.0.world_mut();
    let listener = server_world
        .spawn((Server::new(Instant::now()), AeronetRepliconServer))
        .id();
    server_world.spawn((ChildOf(listener), server));
}

fn step_server(world: &mut World) {
    let mut server = world.non_send_mut::<ServerApp>();
    server.0.update();
    let server_world = server.0.world_mut();
    let mut squares: Vec<Square> = server_world.query::<&Square>().iter(server_world).copied().collect();
    squares.sort_by_key(|square| square.id);
    world.resource_mut::<Truth>().0 = squares;
}

// --- the server ---

#[derive(Resource, Default)]
struct PlayerSteer(Vec2);

fn spawn_squares(mut commands: Commands) {
    for id in 0..5 {
        commands.spawn((Replicated, Square { id, position: Vec2::ZERO }));
    }
}

fn read_steer(mut steers: MessageReader<FromClient<Steer>>, mut steer: ResMut<PlayerSteer>) {
    if let Some(latest) = steers.read().last() {
        steer.0 = latest.message.0;
    }
}

fn move_squares(time: Res<Time>, steer: Res<PlayerSteer>, mut squares: Query<&mut Square>) {
    let t = time.elapsed_secs();
    for mut square in &mut squares {
        square.position = if square.id == 0 {
            (square.position + steer.0 * PLAYER_SPEED * time.delta_secs()).clamp(Vec2::splat(-ARENA), Vec2::splat(ARENA))
        } else {
            // Orbits of different sizes and speeds, one of them a quick zigzag.
            let i = square.id as f32;
            let radius = 40.0 + 30.0 * i;
            let speed = if square.id % 2 == 0 { 1.4 / i } else { -2.2 / i };
            let wobble = if square.id == 4 { Vec2::new(0.0, 25.0 * (t * 9.0).sin()) } else { Vec2::ZERO };
            Vec2::from_angle(t * speed) * radius + wobble
        };
    }
}

// --- the client ---

fn steer(keys: Res<ButtonInput<KeyCode>>, mut steers: MessageWriter<Steer>) {
    let axis = |minus: [KeyCode; 2], plus: [KeyCode; 2]| {
        keys.any_pressed(plus) as i8 as f32 - keys.any_pressed(minus) as i8 as f32
    };
    let direction = Vec2::new(
        axis([KeyCode::ArrowLeft, KeyCode::KeyA], [KeyCode::ArrowRight, KeyCode::KeyD]),
        axis([KeyCode::ArrowDown, KeyCode::KeyS], [KeyCode::ArrowUp, KeyCode::KeyW]),
    );
    steers.write(Steer(direction.normalize_or_zero()));
}

fn color(id: u8) -> Color {
    match id {
        0 => Color::WHITE,
        _ => Color::hsl(36.0 + 72.0 * id as f32, 0.75, 0.6),
    }
}

/// The server's squares, on the left.
#[derive(Component)]
struct TrueSquare(u8);

fn scene(mut commands: Commands) {
    // Off to the left, so the panel has room beside the two panes.
    commands.spawn((
        Camera2d,
        Projection::Orthographic(OrthographicProjection {
            scaling_mode: ScalingMode::AutoMin { min_width: 1180.0, min_height: 470.0 },
            ..OrthographicProjection::default_2d()
        }),
        Transform::from_xyz(-150.0, 0.0, 0.0),
    ));
    let label = |text: &str, at: Vec2| {
        (
            Text2d::new(text),
            TextFont::from_font_size(15.0),
            TextColor(Color::srgb(0.75, 0.78, 0.85)),
            Transform::from_translation(at.extend(1.0)),
        )
    };
    commands.spawn(label("server: the truth", SERVER_PANE + Vec2::new(0.0, ARENA + SQUARE + 18.0)));
    commands.spawn(label("client: through the link", CLIENT_PANE + Vec2::new(0.0, ARENA + SQUARE + 18.0)));
    commands.spawn(label(
        "arrows / WASD steer the white square    F2 panel",
        Vec2::new(0.0, -ARENA - SQUARE - 30.0),
    ));
    for id in 0..5 {
        commands.spawn((TrueSquare(id), Sprite::from_color(color(id), Vec2::splat(SQUARE)), Transform::default()));
    }
}

fn show_truth(truth: Res<Truth>, mut squares: Query<(&TrueSquare, &mut Transform)>) {
    for (TrueSquare(id), mut transform) in &mut squares {
        if let Some(square) = truth.0.iter().find(|square| square.id == *id) {
            transform.translation = (SERVER_PANE + square.position).extend(0.0);
        }
    }
}

/// Replicated squares arrive bare: give them a look.
fn dress_replicated(add: On<Add, Square>, squares: Query<&Square>, mut commands: Commands) {
    let id = squares.get(add.entity).map_or(0, |square| square.id);
    commands
        .entity(add.entity)
        .insert((Sprite::from_color(color(id), Vec2::splat(SQUARE)), Transform::default()));
}

fn show_replicated(mut squares: Query<(&Square, &mut Transform), With<Remote>>) {
    for (square, mut transform) in &mut squares {
        transform.translation = (CLIENT_PANE + square.position).extend(0.0);
    }
}

/// The arenas, and on the client's side the truth outlined, tied to what the
/// client sees by a line: its length is the lag.
fn draw(mut gizmos: Gizmos, truth: Res<Truth>, seen: Query<&Square, With<Remote>>) {
    let frame = Color::srgb(0.3, 0.33, 0.4);
    let arena = Vec2::splat(2.0 * (ARENA + SQUARE));
    gizmos.rect_2d(SERVER_PANE, arena, frame);
    gizmos.rect_2d(CLIENT_PANE, arena, frame);
    for square in &truth.0 {
        let at = CLIENT_PANE + square.position;
        gizmos.rect_2d(at, Vec2::splat(SQUARE + 6.0), color(square.id).with_alpha(0.6));
        if let Some(seen) = seen.iter().find(|seen| seen.id == square.id) {
            gizmos.line_2d(at, CLIENT_PANE + seen.position, color(square.id).with_alpha(0.4));
        }
    }
}
