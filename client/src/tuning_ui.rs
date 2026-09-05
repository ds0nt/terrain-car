use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPlugin, EguiPrimaryContextPass};
use bevy_replicon::prelude::ClientTriggerExt;
use shared::car_physics::{
    CarChassis, BRAKE_FORCE_RANGE, DAMPER_RANGE, ENGINE_FORCE_RANGE, MAX_STEER_RAD_RANGE,
    SPRING_STIFFNESS_RANGE, TRACTION_RANGE,
};
use shared::protocol::TuneCarMsg;

use crate::car::LocalCar;

/// Live suspension/engine tuning panel (`Tab` to toggle). Self-tuning
/// only: it reads/writes the local player's own `CarChassis` directly for
/// instant feedback (same "predict locally, let the server's echo confirm
/// it" pattern the rest of this game already uses — see `TuneCarMsg`'s
/// docs) and sends a `TuneCarMsg` alongside so the server's authoritative
/// copy — now continuously replicated — updates and reaches every other
/// connected client too.
pub struct TuningUiPlugin;

impl Plugin for TuningUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TuningPanelOpen>()
            .add_plugins(EguiPlugin::default())
            .add_systems(Update, toggle_panel)
            .add_systems(EguiPrimaryContextPass, draw_tuning_panel);
    }
}

#[derive(Resource, Default)]
struct TuningPanelOpen(bool);

fn toggle_panel(keyboard: Res<ButtonInput<KeyCode>>, mut open: ResMut<TuningPanelOpen>) {
    if keyboard.just_pressed(KeyCode::Tab) {
        open.0 = !open.0;
    }
}

fn draw_tuning_panel(
    mut contexts: EguiContexts,
    open: Res<TuningPanelOpen>,
    mut commands: Commands,
    mut chassis_q: Query<&mut CarChassis, With<LocalCar>>,
) -> Result {
    if !open.0 {
        return Ok(());
    }
    let Ok(mut chassis) = chassis_q.single_mut() else {
        return Ok(());
    };

    let mut changed = false;
    egui::Window::new("Tuning (Tab to close)").show(contexts.ctx_mut()?, |ui| {
        changed |= ui
            .add(
                egui::Slider::new(
                    &mut chassis.spring_stiffness,
                    SPRING_STIFFNESS_RANGE.0..=SPRING_STIFFNESS_RANGE.1,
                )
                .text("Spring stiffness"),
            )
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut chassis.damper, DAMPER_RANGE.0..=DAMPER_RANGE.1).text("Damper"))
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut chassis.engine_force, ENGINE_FORCE_RANGE.0..=ENGINE_FORCE_RANGE.1)
                    .text("Engine force"),
            )
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut chassis.brake_force, BRAKE_FORCE_RANGE.0..=BRAKE_FORCE_RANGE.1)
                    .text("Brake force"),
            )
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut chassis.traction, TRACTION_RANGE.0..=TRACTION_RANGE.1).text("Traction"))
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut chassis.max_steer_rad, MAX_STEER_RAD_RANGE.0..=MAX_STEER_RAD_RANGE.1)
                    .text("Max steer (rad)"),
            )
            .changed();
    });

    if changed {
        commands.client_trigger(TuneCarMsg {
            spring_stiffness: chassis.spring_stiffness,
            damper: chassis.damper,
            engine_force: chassis.engine_force,
            brake_force: chassis.brake_force,
            traction: chassis.traction,
            max_steer_rad: chassis.max_steer_rad,
        });
    }

    Ok(())
}
