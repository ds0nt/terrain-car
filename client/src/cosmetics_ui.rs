use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPrimaryContextPass};
use bevy_replicon::prelude::ClientTriggerExt;
use shared::protocol::{CarCosmetics, SetCosmeticsMsg};

use crate::car::LocalCar;

/// `V` opens a small panel to pick your own car's paint color and toggle
/// a decorative bow — purely cosmetic, no cost, no gameplay effect.
/// Doesn't register `bevy_egui::EguiPlugin` itself — `auth_ui.rs` already
/// does that once for the whole app.
pub struct CosmeticsUiPlugin;

impl Plugin for CosmeticsUiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CosmeticsPanelOpen>()
            .add_systems(Update, toggle_panel)
            .add_systems(EguiPrimaryContextPass, draw_cosmetics_panel);
    }
}

#[derive(Resource, Default)]
struct CosmeticsPanelOpen(bool);

fn toggle_panel(keyboard: Res<ButtonInput<KeyCode>>, mut open: ResMut<CosmeticsPanelOpen>) {
    if keyboard.just_pressed(KeyCode::KeyV) {
        open.0 = !open.0;
    }
}

/// Predicts locally (mutates the local car's own `CarCosmetics` directly,
/// same "instant feedback, server echo confirms" pattern the old tuning
/// panel used) and sends `SetCosmeticsMsg` alongside so the server's
/// authoritative copy — and therefore every other client — updates too.
fn draw_cosmetics_panel(
    mut contexts: EguiContexts,
    open: Res<CosmeticsPanelOpen>,
    mut commands: Commands,
    mut car_q: Query<&mut CarCosmetics, With<LocalCar>>,
) -> Result {
    if !open.0 {
        return Ok(());
    }
    let Ok(mut cosmetics) = car_q.single_mut() else {
        return Ok(());
    };

    let mut changed = false;
    let mut use_custom_color = cosmetics.custom_color.is_some();
    let mut rgb = cosmetics.custom_color.unwrap_or([0.8, 0.2, 0.2]);

    egui::Window::new("Cosmetics (V to close)").show(contexts.ctx_mut()?, |ui| {
        changed |= ui.checkbox(&mut use_custom_color, "Custom paint color").changed();
        ui.add_enabled_ui(use_custom_color, |ui| {
            changed |= ui.color_edit_button_rgb(&mut rgb).changed();
        });
        changed |= ui.checkbox(&mut cosmetics.has_bow, "Bow on top").changed();
    });

    if changed {
        cosmetics.custom_color = use_custom_color.then_some(rgb);
        commands.client_trigger(SetCosmeticsMsg {
            custom_color: cosmetics.custom_color,
            has_bow: cosmetics.has_bow,
        });
    }

    Ok(())
}
