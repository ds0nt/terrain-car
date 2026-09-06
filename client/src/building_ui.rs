use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPrimaryContextPass};
use bevy_replicon::prelude::ClientTriggerExt;
use shared::buildings::BuildingKind;
use shared::protocol::{RecallToHangarMsg, Wallet};

use crate::building_placement::SelectBuildingKind;
use crate::building_render::base_color_for_kind;
use crate::car::LocalCar;
use crate::owner_color::to_egui_color32;

/// Always-visible build bar pinned to the bottom of the screen, plus the
/// recall-to-Hangar binding (`H`, usable anytime, not just here). Clicking
/// a kind doesn't place it directly — it fires `SelectBuildingKind`, which
/// hands off to `building_placement.rs`'s mouse-raycast ghost/click flow
/// for actually choosing where (and for a Ramp, which way) it goes.
///
/// Doesn't register `bevy_egui::EguiPlugin` itself — `auth_ui.rs`
/// (registered earlier in `main.rs`) already does, and Bevy panics on a
/// duplicate plugin registration.
pub struct BuildingUiPlugin;

impl Plugin for BuildingUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, send_recall_input)
            .add_systems(EguiPrimaryContextPass, draw_build_bar);
    }
}

fn send_recall_input(keyboard: Res<ButtonInput<KeyCode>>, mut commands: Commands) {
    if keyboard.just_pressed(KeyCode::KeyH) {
        commands.client_trigger(RecallToHangarMsg);
    }
}

const BUILDING_KINDS: [BuildingKind; 5] = [
    BuildingKind::Hangar,
    BuildingKind::EnergyGenerator,
    BuildingKind::ExtractionFacility,
    BuildingKind::Ramp,
    BuildingKind::LandFactory,
];

const ICON_SIZE: f32 = 48.0;

/// One icon-placeholder button: a colored swatch (matching the kind's own
/// in-world color — see `base_color_for_kind`) plus a label and cost line
/// underneath. Returns whether it was clicked this frame.
fn building_button(ui: &mut egui::Ui, kind: BuildingKind, affordable: bool) -> bool {
    let (cost_energy, cost_ore) = kind.cost();
    let mut swatch = to_egui_color32(base_color_for_kind(kind));
    if !affordable {
        swatch = swatch.linear_multiply(0.35);
    }

    let mut clicked = false;
    ui.vertical(|ui| {
        if !affordable {
            ui.disable();
        }
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ICON_SIZE, ICON_SIZE), egui::Sense::click());
        ui.painter().rect_filled(rect, 4.0, swatch);
        ui.painter().rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(1.0, egui::Color32::from_gray(200)),
            egui::StrokeKind::Outside,
        );
        ui.label(format!("{kind:?}"));
        ui.label(format!("{cost_energy:.0}⚡ {cost_ore:.0}⛏"));
        clicked = response.clicked();
    });
    clicked
}

fn draw_build_bar(
    mut contexts: EguiContexts,
    mut select_events: MessageWriter<SelectBuildingKind>,
    car_q: Query<&Wallet, With<LocalCar>>,
) -> Result {
    let Ok(wallet) = car_q.single() else {
        return Ok(());
    };

    egui::Area::new(egui::Id::new("build_bar"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -14.0))
        .show(contexts.ctx_mut()?, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("Energy: {:.0}   Ore: {:.0}", wallet.energy, wallet.ore));
                    ui.separator();
                    for kind in BUILDING_KINDS {
                        let (cost_energy, cost_ore) = kind.cost();
                        let affordable = wallet.energy >= cost_energy && wallet.ore >= cost_ore;
                        if building_button(ui, kind, affordable) {
                            select_events.write(SelectBuildingKind(kind));
                        }
                    }
                    ui.separator();
                    ui.label("H: recall to Hangar");
                });
            });
        });

    Ok(())
}
