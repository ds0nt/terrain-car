use bevy::prelude::Color;
use bevy_egui::egui;
use uuid::Uuid;

pub(crate) use shared::owner::seed_from_uuid;

/// Deterministic seed -> vibrant paint color. Every entity with the same
/// seed gets the same color on every viewer — used for a car's paint
/// (`car_render.rs`) and, via `seed_from_uuid`, for tinting a player's
/// buildings/villagers the same color too, so a player's whole footprint
/// on the map reads as one consistent identity. Fixed saturation/
/// lightness, varying only hue, so every color reads clearly against the
/// terrain regardless of which hue it lands on.
pub(crate) fn color_from_seed(seed: u32) -> Color {
    let hue = (seed.wrapping_mul(2_654_435_761) % 360) as f32;
    Color::hsl(hue, 0.75, 0.5)
}

/// Convenience: hash straight from a player's account id to their paint
/// color in one call, for the (common) case of not needing the raw seed.
pub(crate) fn color_for_owner(id: Uuid) -> Color {
    color_from_seed(seed_from_uuid(id))
}

/// `bevy::Color` -> `egui::Color32` — every `egui`-drawn owner-color UI
/// (minimap blips, build-bar icons, the players panel) needs this same
/// conversion, so it lives here rather than being reimplemented per file.
pub(crate) fn to_egui_color32(color: Color) -> egui::Color32 {
    let srgba = color.to_srgba();
    egui::Color32::from_rgb(
        (srgba.red * 255.0) as u8,
        (srgba.green * 255.0) as u8,
        (srgba.blue * 255.0) as u8,
    )
}
