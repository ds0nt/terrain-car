use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPrimaryContextPass};
use bevy_replicon::prelude::ClientTriggerExt;
use shared::buildings::BuildingKind;
use shared::protocol::{PlaceBuildingMsg, RecallToHangarMsg, Wallet};
use shared::worldspace::WorldOrigin;

use crate::car::LocalCar;

/// Live build menu (`B` to toggle) plus the recall-to-Hangar binding (`H`,
/// not gated behind the menu — meant to be usable mid-drive). All the
/// actual placement logic (funds, deposit proximity, distance-from-car) is
/// server-side (`server/src/economy.rs`); this is purely "press B, click
/// Place, send the message."
///
/// Doesn't register `bevy_egui::EguiPlugin` itself — `tuning_ui.rs`
/// (registered earlier in `main.rs`) already does, and Bevy panics on a
/// duplicate plugin registration.
pub struct BuildingUiPlugin;

impl Plugin for BuildingUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BuildMenuOpen>()
            .add_systems(Update, (toggle_build_menu, send_recall_input))
            .add_systems(EguiPrimaryContextPass, draw_build_menu);
    }
}

#[derive(Resource, Default)]
struct BuildMenuOpen(bool);

fn toggle_build_menu(keyboard: Res<ButtonInput<KeyCode>>, mut open: ResMut<BuildMenuOpen>) {
    if keyboard.just_pressed(KeyCode::KeyB) {
        open.0 = !open.0;
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

fn draw_build_menu(
    mut contexts: EguiContexts,
    open: Res<BuildMenuOpen>,
    mut commands: Commands,
    origin: Res<WorldOrigin>,
    car_q: Query<(&Transform, &Wallet), With<LocalCar>>,
) -> Result {
    if !open.0 {
        return Ok(());
    }
    let Ok((transform, wallet)) = car_q.single() else {
        return Ok(());
    };
    let true_pos = origin.to_true(transform.translation);
    // Captured so a Ramp faces the way the car was pointed at placement —
    // see PlaceBuildingMsg's docs; every other kind ignores this.
    let (rotation_y, _, _) = transform.rotation.to_euler(EulerRot::YXZ);

    egui::Window::new("Build (B to close)").show(contexts.ctx_mut()?, |ui| {
        ui.label(format!("Energy: {:.0}   Ore: {:.0}", wallet.energy, wallet.ore));
        ui.separator();
        for kind in BUILDING_KINDS {
            let (cost_energy, cost_ore) = kind.cost();
            let affordable = wallet.energy >= cost_energy && wallet.ore >= cost_ore;
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{kind:?}  ({:.0}s build)  cost: {cost_energy:.0} energy, {cost_ore:.0} ore",
                    kind.build_time_secs(),
                ));
                if ui.add_enabled(affordable, egui::Button::new("Place here")).clicked() {
                    commands.client_trigger(PlaceBuildingMsg {
                        kind,
                        true_x: true_pos.x,
                        true_z: true_pos.z,
                        rotation_y,
                    });
                }
            });
        }
        ui.separator();
        ui.label("H: recall your car to your Hangar (anytime, not just here)");
    });

    Ok(())
}
