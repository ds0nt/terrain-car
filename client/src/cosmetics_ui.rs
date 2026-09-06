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

/// A selectable swatch for the paint color — a handful of good presets
/// plus a full custom picker, laid out as a proper garage screen rather
/// than a cramped tool window.
const PRESET_COLORS: [(&str, [f32; 3]); 6] = [
    ("Crimson", [0.8, 0.15, 0.15]),
    ("Amber", [0.9, 0.6, 0.1]),
    ("Emerald", [0.15, 0.7, 0.35]),
    ("Azure", [0.15, 0.45, 0.9]),
    ("Violet", [0.55, 0.25, 0.85]),
    ("Slate", [0.3, 0.32, 0.36]),
];

/// Predicts locally (mutates the local car's own `CarCosmetics` directly,
/// same "instant feedback, server echo confirms" pattern the old tuning
/// panel used) and sends `SetCosmeticsMsg` alongside so the server's
/// authoritative copy — and therefore every other client — updates too.
///
/// Drawn as a full-screen "garage" takeover (a dimming backdrop behind a
/// large centered panel) rather than a small floating tool window — this
/// is meant to feel like stepping into a dedicated customization screen,
/// not tweaking a debug panel while still half-driving.
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

    let screen_rect = contexts.ctx_mut()?.viewport_rect();
    egui::Area::new(egui::Id::new("cosmetics_backdrop"))
        .order(egui::Order::Background)
        .fixed_pos(screen_rect.min)
        .show(contexts.ctx_mut()?, |ui| {
            ui.painter().rect_filled(screen_rect, 0.0, egui::Color32::from_black_alpha(215));
        });

    let mut changed = false;
    let mut use_custom_color = cosmetics.custom_color.is_some();
    let mut rgb = cosmetics.custom_color.unwrap_or(PRESET_COLORS[0].1);

    let panel_size = egui::vec2(
        (screen_rect.width() * 0.55).clamp(460.0, 820.0),
        (screen_rect.height() * 0.6).clamp(360.0, 620.0),
    );
    egui::Window::new("Garage")
        .collapsible(false)
        .resizable(false)
        .fixed_size(panel_size)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(contexts.ctx_mut()?, |ui| {
            ui.add_space(6.0);
            ui.vertical_centered(|ui| ui.heading("Customize Your Car"));
            ui.add_space(16.0);
            ui.separator();
            ui.add_space(16.0);

            ui.label("Paint");
            ui.add_space(6.0);
            changed |= ui.checkbox(&mut use_custom_color, "Use custom paint").changed();
            ui.add_space(8.0);
            ui.add_enabled_ui(use_custom_color, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (name, color) in PRESET_COLORS {
                        let is_selected = (rgb[0] - color[0]).abs() < 0.01
                            && (rgb[1] - color[1]).abs() < 0.01
                            && (rgb[2] - color[2]).abs() < 0.01;
                        let swatch = egui::Color32::from_rgb(
                            (color[0] * 255.0) as u8,
                            (color[1] * 255.0) as u8,
                            (color[2] * 255.0) as u8,
                        );
                        let button = egui::Button::new("").fill(swatch).min_size(egui::vec2(36.0, 36.0));
                        let button = if is_selected {
                            button.stroke(egui::Stroke::new(3.0, egui::Color32::WHITE))
                        } else {
                            button
                        };
                        if ui.add(button).on_hover_text(name).clicked() {
                            rgb = color;
                            changed = true;
                        }
                    }
                    ui.add_space(10.0);
                    ui.label("Custom:");
                    changed |= ui.color_edit_button_rgb(&mut rgb).changed();
                });
            });

            ui.add_space(20.0);
            ui.separator();
            ui.add_space(16.0);

            ui.label("Accessories");
            ui.add_space(6.0);
            changed |= ui.checkbox(&mut cosmetics.has_bow, "🎀  Bow on top").changed();

            ui.add_space(20.0);
            ui.separator();
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("V to close").weak());
            });
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
