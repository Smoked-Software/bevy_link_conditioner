//! The live panel: one egui window that edits the [`DefaultLinkConditioner`]
//! (so every session follows it) and shows what the links are doing.

use core::time::Duration;

use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass, egui};

use crate::{
    ConditionedLink, DefaultLinkConditioner, Direction, Link, LinkConditioner, Mode, PRESETS,
};

/// The panel. Needs bevy_egui's `EguiPlugin` and [`crate::LinkConditionerPlugin`].
pub struct LinkConditionerUiPlugin {
    /// Shows and hides the panel.
    pub toggle: KeyCode,
    /// Open at start.
    pub open: bool,
}

impl Default for LinkConditionerUiPlugin {
    fn default() -> Self {
        Self {
            toggle: KeyCode::F2,
            open: false,
        }
    }
}

/// The panel's own state.
#[derive(Resource)]
pub struct LinkConditionerUi {
    pub open: bool,
    pub toggle: KeyCode,
    /// Edit both directions as one.
    pub same_both_ways: bool,
}

impl Plugin for LinkConditionerUiPlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<DefaultLinkConditioner>() {
            // Something to edit, passing everything through until switched on.
            app.insert_resource(DefaultLinkConditioner(LinkConditioner {
                enabled: false,
                ..default()
            }));
        }
        app.insert_resource(LinkConditionerUi {
            open: self.open,
            toggle: self.toggle,
            same_both_ways: true,
        })
        .add_systems(Update, toggle)
        .add_systems(EguiPrimaryContextPass, panel);
    }
}

fn toggle(keys: Res<ButtonInput<KeyCode>>, mut ui: ResMut<LinkConditionerUi>) {
    if keys.just_pressed(ui.toggle) {
        ui.open = !ui.open;
    }
}

const UP_COLOR: egui::Color32 = egui::Color32::from_rgb(90, 170, 255);
const DOWN_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 160, 60);

fn panel(
    mut contexts: EguiContexts,
    mut state: ResMut<LinkConditionerUi>,
    mut default: ResMut<DefaultLinkConditioner>,
    links: Query<&ConditionedLink>,
) -> Result {
    if !state.open {
        return Ok(());
    }
    let mut open = true;
    let mut edit = default.0.clone();
    let title = format!("Link conditioner ({:?})", state.toggle);
    egui::Window::new(title)
        .id(egui::Id::new("link_conditioner"))
        .open(&mut open)
        .default_width(340.0)
        .show(contexts.ctx_mut()?, |ui| {
            settings(ui, &mut state, &mut edit);
            ui.separator();
            match links.iter().next() {
                Some(link) => live(ui, link),
                None => {
                    ui.weak(if edit.enabled {
                        "No session yet."
                    } else {
                        "Off: packets pass straight through."
                    });
                }
            }
            ui.separator();
            let spec = edit.to_string();
            ui.horizontal(|ui| {
                if ui
                    .small_button("copy")
                    .on_hover_text("--net=<this> or NET_CONDITION=<this>")
                    .clicked()
                {
                    ui.ctx().copy_text(spec.clone());
                }
                ui.code(&spec);
            });
            egui::CollapsingHeader::new("netem equivalent").show(ui, |ui| {
                let netem = edit.netem();
                if ui.small_button("copy").clicked() {
                    ui.ctx().copy_text(netem.clone());
                }
                ui.code(netem);
            });
        });
    state.open = open;
    default.set_if_neq(DefaultLinkConditioner(edit));
    Ok(())
}

fn settings(ui: &mut egui::Ui, state: &mut LinkConditionerUi, edit: &mut LinkConditioner) {
    ui.horizontal(|ui| {
        ui.checkbox(&mut edit.enabled, "on");
        ui.separator();
        ui.selectable_value(&mut edit.mode, Mode::Stream, "stream").on_hover_text(
            "WebSocket/TCP: nothing lost or reordered; a loss stalls the direction until the retransmission lands",
        );
        ui.selectable_value(&mut edit.mode, Mode::Datagram, "datagram")
            .on_hover_text("WebTransport/UDP: losses are gone, jitter reorders, a full queue drops");
    });
    ui.horizontal(|ui| {
        let current = PRESETS
            .into_iter()
            .find(|name| {
                Direction::preset(name)
                    .is_some_and(|preset| edit.up == preset && edit.down == preset)
            })
            .unwrap_or("custom");
        egui::ComboBox::from_id_salt("preset")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for name in PRESETS {
                    if ui.selectable_label(name == current, name).clicked() {
                        let preset = Direction::preset(name).expect("listed");
                        (edit.up, edit.down) = (preset, preset);
                    }
                }
            });
        ui.label("seed");
        ui.add(egui::DragValue::new(&mut edit.seed));
        ui.checkbox(&mut state.same_both_ways, "same both ways");
    });
    if state.same_both_ways {
        direction(ui, "both", &mut edit.up, edit.mode);
        edit.down = edit.up;
    } else {
        egui::CollapsingHeader::new(egui::RichText::new("up: client to server").color(UP_COLOR))
            .default_open(true)
            .show(ui, |ui| direction(ui, "up", &mut edit.up, edit.mode));
        egui::CollapsingHeader::new(
            egui::RichText::new("down: server to client").color(DOWN_COLOR),
        )
        .default_open(true)
        .show(ui, |ui| direction(ui, "down", &mut edit.down, edit.mode));
    }
}

fn direction(ui: &mut egui::Ui, id: &str, direction: &mut Direction, mode: Mode) {
    egui::Grid::new(id).num_columns(2).show(ui, |ui| {
        millis(ui, "latency", &mut direction.latency, 1000.0);
        millis(ui, "jitter", &mut direction.jitter, 200.0);
        percent(ui, "correlation", &mut direction.correlation, 100.0);
        percent(ui, "loss", &mut direction.loss, 30.0);
        millis(ui, "burst", &mut direction.burst, 1000.0);
        ui.label("bandwidth");
        ui.add(
            egui::Slider::new(&mut direction.bandwidth, 0..=20_000)
                .logarithmic(true)
                .suffix(" kbit/s")
                .custom_formatter(|kbit, _| {
                    if kbit == 0.0 {
                        "off".into()
                    } else {
                        format!("{kbit:.0}")
                    }
                }),
        );
        ui.end_row();
        millis(ui, "outage every", &mut direction.outage_every, 60_000.0);
        millis(ui, "outage hold", &mut direction.outage_hold, 5000.0);
        match mode {
            Mode::Stream => {
                ui.label("send buffer");
                ui.add(
                    egui::Slider::new(&mut direction.send_buffer, 1024..=256 * 1024)
                        .logarithmic(true)
                        .suffix(" B"),
                );
                ui.end_row();
            }
            Mode::Datagram => {
                millis(ui, "queue", &mut direction.queue, 1000.0);
                percent(ui, "reorder", &mut direction.reorder, 30.0);
                percent(ui, "duplicate", &mut direction.duplicate, 30.0);
            }
        }
    });
}

fn millis(ui: &mut egui::Ui, label: &str, value: &mut Duration, max: f64) {
    ui.label(label);
    let mut ms = value.as_secs_f64() * 1000.0;
    let slider = egui::Slider::new(&mut ms, 0.0..=max)
        .suffix(" ms")
        .max_decimals(0);
    if ui
        .add(if max > 2000.0 {
            slider.logarithmic(true)
        } else {
            slider
        })
        .changed()
    {
        *value = Duration::from_millis(ms.round() as u64);
    }
    ui.end_row();
}

fn percent(ui: &mut egui::Ui, label: &str, value: &mut f32, max: f32) {
    ui.label(label);
    ui.add(
        egui::Slider::new(value, 0.0..=max)
            .suffix(" %")
            .max_decimals(1),
    );
    ui.end_row();
}

/// What the links hold and have done, and the last few seconds of delay.
fn live(ui: &mut egui::Ui, link: &ConditionedLink) {
    let (up, down) = (link.up().stats(), link.down().stats());
    egui::Grid::new("live")
        .num_columns(3)
        .striped(true)
        .show(ui, |ui| {
            ui.label("");
            ui.colored_label(UP_COLOR, "up");
            ui.colored_label(DOWN_COLOR, "down");
            ui.end_row();
            let mut row = |label: &str, up: String, down: String| {
                ui.label(label);
                ui.monospace(up);
                ui.monospace(down);
                ui.end_row();
            };
            let ms = |duration: Duration| format!("{:.0} ms", duration.as_secs_f64() * 1000.0);
            row("delay", ms(up.last_delay), ms(down.last_delay));
            row("stall", ms(up.stall), ms(down.stall));
            let held = |stats: &crate::Stats| {
                format!(
                    "{} / {:.1} KiB",
                    stats.queued,
                    stats.queued_bytes as f64 / 1024.0
                )
            };
            row("held", held(up), held(down));
            row("sent", up.sent.to_string(), down.sent.to_string());
            row("lost on wire", up.lost.to_string(), down.lost.to_string());
            if link.up().mode() == Mode::Stream {
                row(
                    "retransmits",
                    up.retransmits.to_string(),
                    down.retransmits.to_string(),
                );
                row(
                    "timeouts",
                    up.timeouts.to_string(),
                    down.timeouts.to_string(),
                );
                row(
                    "send door drops",
                    up.dropped.to_string(),
                    down.dropped.to_string(),
                );
            } else {
                row(
                    "queue drops",
                    up.dropped.to_string(),
                    down.dropped.to_string(),
                );
                row(
                    "duplicated",
                    up.duplicated.to_string(),
                    down.duplicated.to_string(),
                );
            }
        });
    sparkline(
        ui,
        link.now(),
        [(link.up(), UP_COLOR), (link.down(), DOWN_COLOR)],
    );
}

/// One-way delay of each packet delivered in the last five seconds.
fn sparkline(
    ui: &mut egui::Ui,
    now: Duration,
    links: [(&Link<aeronet_io::bytes::Bytes>, egui::Color32); 2],
) {
    let size = egui::vec2(ui.available_width(), 70.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let window = crate::link::HISTORY.as_secs_f32();
    let top = links
        .iter()
        .flat_map(|(link, _)| link.history().map(|(_, delay)| delay.as_secs_f32()))
        .fold(0.05_f32, f32::max)
        * 1.15;
    for (link, color) in links {
        let points: Vec<egui::Pos2> = link
            .history()
            .map(|(at, delay)| {
                let age = now.saturating_sub(at).as_secs_f32() / window;
                egui::pos2(
                    rect.right() - age * rect.width(),
                    rect.bottom() - delay.as_secs_f32() / top * rect.height(),
                )
            })
            .collect();
        painter.add(egui::Shape::line(points, egui::Stroke::new(1.5, color)));
    }
    painter.text(
        rect.left_top() + egui::vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        format!("{:.0} ms, last {window:.0} s", top * 1000.0),
        egui::FontId::monospace(10.0),
        ui.visuals().weak_text_color(),
    );
}
