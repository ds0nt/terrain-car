use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPrimaryContextPass};
use shared::protocol::PlayerInfo;

use crate::owner_color::{color_for_owner, to_egui_color32};
use crate::pings::{RecentPings, PING_TTL_SECS};

/// Connected-players roster plus a short "who pinged, how long ago" list
/// — placed just under the minimap. Player rows come straight off
/// `PlayerInfo`, replicated on every car (see `shared::protocol`'s docs
/// on that component), so this needs no data of its own; the pings list
/// reads `pings.rs`'s `RecentPings` resource, which that module already
/// maintains. Doesn't register `bevy_egui::EguiPlugin` itself —
/// `auth_ui.rs` already does.
pub struct PlayersUiPlugin;

impl Plugin for PlayersUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(EguiPrimaryContextPass, draw_players_panel);
    }
}

/// Below the minimap's own top-right slot (16px inset, 170px tall) plus a
/// small gap.
const PANEL_OFFSET: egui::Vec2 = egui::vec2(-16.0, 16.0 + 170.0 + 12.0);

fn draw_players_panel(
    mut contexts: EguiContexts,
    players: Query<&PlayerInfo>,
    recent_pings: Res<RecentPings>,
    time: Res<Time>,
) -> Result {
    egui::Area::new(egui::Id::new("players_panel")).anchor(egui::Align2::RIGHT_TOP, PANEL_OFFSET).show(
        contexts.ctx_mut()?,
        |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(150.0);
                ui.label("Players");
                ui.separator();
                for info in &players {
                    ui.horizontal(|ui| {
                        let color = to_egui_color32(color_for_owner(info.player_id));
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, color);
                        ui.label(&info.username);
                    });
                }

                let now = time.elapsed_secs();
                let active: Vec<_> =
                    recent_pings.0.iter().filter(|p| now - p.received_at < PING_TTL_SECS).collect();
                if !active.is_empty() {
                    ui.separator();
                    ui.label("Pings");
                    for entry in active {
                        let ago = (now - entry.received_at) as u32;
                        ui.label(format!("{} — {ago}s ago", entry.username));
                    }
                }
            });
        },
    );

    Ok(())
}
