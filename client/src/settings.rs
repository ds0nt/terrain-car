use bevy::light::DirectionalLightShadowMap;
use bevy::prelude::*;
use bevy::window::{MonitorSelection, PresentMode, PrimaryWindow, WindowMode};
use bevy_egui::{egui, EguiContexts, EguiPrimaryContextPass};

use crate::camera::CarCamera;
use crate::chat::ChatOpen;
use crate::cosmetics_ui::CosmeticsPanelOpen;
use crate::terrain::ViewDistance;

/// `F10` opens/closes a settings menu — graphics options for now, plus a
/// shortcut into the existing cosmetics ("Garage") panel rather than
/// duplicating it. `F10`, not `Escape`: `Escape` is already spoken for by
/// three *other* independent systems (`chat.rs` closing the chat box,
/// `building_placement.rs` canceling an in-progress placement,
/// `selection.rs` clearing the current selection, `pilot.rs` closing the
/// build UI) — making it *also* open this would need those four to agree
/// on priority (only open Settings if none of them had anything to close),
/// which a single dedicated key sidesteps entirely. `Escape` still closes
/// this menu specifically once it's open, the same "back out of whatever's
/// topmost" convention every one of those other systems already follows on
/// its own — that's a one-way check (only ever closes, never opens), so it
/// can't conflict with any of them either.
pub struct SettingsPlugin;

impl Plugin for SettingsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SettingsOpen>()
            .add_systems(Update, toggle_settings)
            .add_systems(EguiPrimaryContextPass, draw_settings);
    }
}

/// `pub` (field included) so `pilot.rs`'s `manage_cursor_confinement` can
/// free the cursor while this is open — without it, opening Settings while
/// on foot/flying left the cursor locked and hidden for mouse-look,
/// meaning none of this menu's own checkboxes/sliders could actually be
/// clicked.
#[derive(Resource, Default)]
pub struct SettingsOpen(pub bool);

fn toggle_settings(keyboard: Res<ButtonInput<KeyCode>>, chat_open: Res<ChatOpen>, mut open: ResMut<SettingsOpen>) {
    if chat_open.0 {
        return;
    }
    if keyboard.just_pressed(KeyCode::F10) {
        open.0 = !open.0;
    } else if open.0 && keyboard.just_pressed(KeyCode::Escape) {
        open.0 = false;
    }
}

/// Degrees, not radians — `PerspectiveProjection::fov` is radians
/// internally, but nobody thinks in radians when dragging a slider.
const FOV_RANGE_DEG: std::ops::RangeInclusive<f32> = 60.0..=110.0;
/// In chunks (see `ViewDistance`) — 2 is noticeably short-sighted, 8 is a
/// 17x17 grid (289 chunks with colliders), already a real cost on modest
/// hardware, which is exactly why this is a *choice* now instead of a
/// fixed constant.
const VIEW_DISTANCE_RANGE: std::ops::RangeInclusive<i64> = 2..=8;
/// A handful of common resolutions rather than a free-typed width/height —
/// covers the vast majority of real displays without needing input
/// validation for garbage values.
const RESOLUTIONS: [(u32, u32); 5] = [(1280, 720), (1600, 900), (1920, 1080), (2560, 1440), (3840, 2160)];
/// Mirrors `lighting.rs`'s own shadow tiers — `Off` skips real-time shadow
/// mapping entirely (`DirectionalLight::shadow_maps_enabled`), the other
/// three just pick a `DirectionalLightShadowMap` resolution. Low/Medium/
/// High rather than a free-typed pixel size for the same reason the
/// resolution picker above uses fixed choices instead of raw numbers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShadowQuality {
    Off,
    Low,
    Medium,
    High,
}

impl ShadowQuality {
    fn from_current(enabled: bool, size: usize) -> Self {
        if !enabled {
            return Self::Off;
        }
        match size {
            0..=1024 => Self::Low,
            1025..=2048 => Self::Medium,
            _ => Self::High,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }

    fn map_size(self) -> usize {
        match self {
            Self::Off | Self::Low => 1024,
            Self::Medium => 2048,
            Self::High => 4096,
        }
    }
}

/// Drawn as the same kind of full-screen "takeover" (dimming backdrop
/// behind a large centered panel) `cosmetics_ui.rs`'s own Garage screen
/// already uses, for the same reason: this is meant to feel like a real
/// settings screen, not a small floating debug window.
#[allow(clippy::too_many_arguments)]
fn draw_settings(
    mut contexts: EguiContexts,
    mut open: ResMut<SettingsOpen>,
    mut cosmetics_open: ResMut<CosmeticsPanelOpen>,
    mut view_distance: ResMut<ViewDistance>,
    mut shadow_map: ResMut<DirectionalLightShadowMap>,
    mut sun_q: Query<&mut DirectionalLight>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    mut camera_q: Query<&mut Projection, With<CarCamera>>,
) -> Result {
    if !open.0 {
        return Ok(());
    }

    let screen_rect = contexts.ctx_mut()?.viewport_rect();
    egui::Area::new(egui::Id::new("settings_backdrop"))
        .order(egui::Order::Background)
        .fixed_pos(screen_rect.min)
        .show(contexts.ctx_mut()?, |ui| {
            ui.painter().rect_filled(screen_rect, 0.0, egui::Color32::from_black_alpha(215));
        });

    let panel_size = egui::vec2(
        (screen_rect.width() * 0.45).clamp(420.0, 640.0),
        (screen_rect.height() * 0.55).clamp(340.0, 520.0),
    );
    egui::Window::new("Settings")
        .collapsible(false)
        .resizable(false)
        .fixed_size(panel_size)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(contexts.ctx_mut()?, |ui| {
            ui.add_space(6.0);
            ui.vertical_centered(|ui| ui.heading("Settings"));
            ui.add_space(16.0);
            ui.separator();
            ui.add_space(16.0);

            ui.label(egui::RichText::new("Graphics").strong());
            ui.add_space(8.0);
            if let Ok(mut window) = windows.single_mut() {
                let mut fullscreen = !matches!(window.mode, WindowMode::Windowed);
                if ui.checkbox(&mut fullscreen, "Fullscreen").changed() {
                    window.mode = if fullscreen {
                        WindowMode::BorderlessFullscreen(MonitorSelection::Current)
                    } else {
                        WindowMode::Windowed
                    };
                }
                let mut vsync = matches!(window.present_mode, PresentMode::Fifo | PresentMode::FifoRelaxed);
                if ui.checkbox(&mut vsync, "V-Sync").changed() {
                    window.present_mode = if vsync { PresentMode::Fifo } else { PresentMode::Immediate };
                }

                // Only meaningful windowed — a borderless-fullscreen window
                // already tracks the monitor's own native resolution
                // regardless of this, so the picker is disabled rather than
                // silently doing nothing while fullscreen is on.
                ui.add_space(4.0);
                ui.add_enabled_ui(!fullscreen, |ui| {
                    let current = (window.resolution.width() as u32, window.resolution.height() as u32);
                    egui::ComboBox::from_label("Resolution")
                        .selected_text(format!("{}x{}", current.0, current.1))
                        .show_ui(ui, |ui| {
                            for (w, h) in RESOLUTIONS {
                                if ui.selectable_label(current == (w, h), format!("{w}x{h}")).clicked() {
                                    window.resolution.set(w as f32, h as f32);
                                }
                            }
                        });
                });
            }
            if let Ok(mut projection) = camera_q.single_mut()
                && let Projection::Perspective(perspective) = &mut *projection
            {
                let mut fov_deg = perspective.fov.to_degrees();
                ui.add_space(4.0);
                if ui.add(egui::Slider::new(&mut fov_deg, FOV_RANGE_DEG).text("Field of view")).changed() {
                    perspective.fov = fov_deg.to_radians();
                }
            }

            ui.add_space(4.0);
            let mut view_distance_chunks = view_distance.chunks;
            if ui
                .add(egui::Slider::new(&mut view_distance_chunks, VIEW_DISTANCE_RANGE).text("View distance"))
                .changed()
            {
                // `stream_chunks` and its own stale-cleanup both re-read
                // this fresh every frame (see `ViewDistance`'s own docs) —
                // nothing else to poke here, raising or lowering it takes
                // effect on its own starting next frame.
                view_distance.chunks = view_distance_chunks;
            }

            ui.add_space(4.0);
            if let Ok(mut sun) = sun_q.single_mut() {
                let current = ShadowQuality::from_current(sun.shadow_maps_enabled, shadow_map.size);
                let mut selected = current;
                egui::ComboBox::from_label("Shadows")
                    .selected_text(selected.label())
                    .show_ui(ui, |ui| {
                        for option in [ShadowQuality::Off, ShadowQuality::Low, ShadowQuality::Medium, ShadowQuality::High]
                        {
                            ui.selectable_value(&mut selected, option, option.label());
                        }
                    });
                if selected != current {
                    sun.shadow_maps_enabled = selected != ShadowQuality::Off;
                    shadow_map.size = selected.map_size();
                }
            }

            ui.add_space(20.0);
            ui.separator();
            ui.add_space(16.0);

            ui.label(egui::RichText::new("Cosmetics").strong());
            ui.add_space(8.0);
            if ui.add(egui::Button::new("Open Garage (car paint & accessories)").sense(egui::Sense::CLICK)).clicked()
            {
                cosmetics_open.0 = true;
                open.0 = false;
            }

            ui.add_space(20.0);
            ui.separator();
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("F10 or Esc to close").weak());
            });
        });

    Ok(())
}
