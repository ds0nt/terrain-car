use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPrimaryContextPass};
use bevy_replicon::prelude::ClientTriggerExt;
use shared::buildings::{BuildingKind, MAX_VILLAGERS_PER_PLAYER};
use shared::protocol::{
    BuildingSnapshot, DestroyBuildingMsg, QueueVillagerMsg, RecallPlaneMsg, RecallPlayerMsg, RecallToHangarMsg,
    VillagerQueue, VillagerSnapshot, Wallet,
};

use crate::aircraft::DrivingPlaneId;
use crate::auth_ui::LocalPlayerId;
use crate::building_placement::SelectBuildingKind;
use crate::building_render::base_color_for_kind;
use crate::car::DrivingCarId;
use crate::dropship::{send_dropship_recall, DrivingDropshipId};
use crate::owner_color::to_egui_color32;
use crate::player_account::LocalPlayerAccount;
use crate::selection::Selected;
use crate::tank::{send_tank_recall, DrivingTankId};

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
        app.init_resource::<PendingDestroy>()
            .add_systems(Update, send_recall_input)
            .add_systems(EguiPrimaryContextPass, (draw_build_bar, draw_selected_building_panel));
    }
}

/// `H`: recall whichever car you're currently driving to the nearest
/// Hangar you own, whichever plane you're currently flying back to its
/// own Air Factory, or — while on foot in neither — yourself, to whichever
/// owned Hangar/Land Factory/Air Factory is nearest (see
/// `car_sim::apply_recall_to_hangar`/`aircraft::apply_recall_plane`/
/// `car_sim::apply_recall_player`, respectively; the server only actually
/// moves the one that applies). `RecallToHangarMsg`/`RecallPlaneMsg` need
/// `car_id`/`plane_id` (see their own docs on why "every owned one,
/// unconditionally" stopped being what was wanted), so those two only
/// fire when there's actually an id to send; `RecallPlayerMsg` needs none
/// and only makes sense exactly when neither of those does.
#[allow(clippy::too_many_arguments)]
fn send_recall_input(
    keyboard: Res<ButtonInput<KeyCode>>,
    chat_open: Res<crate::chat::ChatOpen>,
    mode: Res<crate::pilot::ControlMode>,
    driving_car: Res<DrivingCarId>,
    driving_plane: Res<DrivingPlaneId>,
    driving_tank: Res<DrivingTankId>,
    driving_dropship: Res<DrivingDropshipId>,
    mut commands: Commands,
) {
    if !chat_open.0 && keyboard.just_pressed(KeyCode::KeyH) {
        if let Some(car_id) = driving_car.0 {
            commands.client_trigger(RecallToHangarMsg { car_id });
        } else if let Some(plane_id) = driving_plane.0 {
            commands.client_trigger(RecallPlaneMsg { plane_id });
        } else if let Some(tank_id) = driving_tank.0 {
            send_tank_recall(&mut commands, tank_id);
        } else if let Some(dropship_id) = driving_dropship.0 {
            send_dropship_recall(&mut commands, dropship_id);
        } else if *mode == crate::pilot::ControlMode::OnFoot {
            commands.client_trigger(RecallPlayerMsg);
        }
    }
}

const BUILDING_KINDS: [BuildingKind; 12] = [
    BuildingKind::Hangar,
    BuildingKind::EnergyGenerator,
    BuildingKind::ExtractionFacility,
    BuildingKind::Ramp,
    BuildingKind::Road,
    BuildingKind::Platform,
    BuildingKind::Wall,
    BuildingKind::LandFactory,
    BuildingKind::AirFactory,
    BuildingKind::WarFactory,
    BuildingKind::Dropyard,
    BuildingKind::Turret,
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
        // `Sense::CLICK`, not `Sense::click()` — the latter also makes the
        // widget keyboard-focusable/tab-navigable, which is exactly what
        // let `Tab` cycle between build-bar icons and a much later,
        // unrelated `Enter`/`Space` press "click" whichever one was left
        // focused (see `chat::clear_stray_ui_focus`'s own docs on the
        // messier fix this replaced). Mouse-only by construction now —
        // there's simply nothing here for `Tab` to ever land on.
        let (rect, response) = ui.allocate_exact_size(egui::vec2(ICON_SIZE, ICON_SIZE), egui::Sense::CLICK);
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
    menu_open: Res<crate::pilot::MenuOpen>,
    mut select_events: MessageWriter<SelectBuildingKind>,
    car_q: Query<&Wallet, With<LocalPlayerAccount>>,
) -> Result {
    // Hidden entirely, not just unclickable, unless `E` (`MenuOpen`) is
    // held/toggled on — reported live as wanting the build UI out of the
    // way by default rather than a permanent fixture at the bottom of the
    // screen.
    if !menu_open.0 {
        return Ok(());
    }
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
                    ui.label("H: recall vehicle to base");
                });
            });
        });

    Ok(())
}

/// Arms (or resets) the two-click destroy confirmation on the currently
/// selected building — see `draw_selected_building_panel`'s destroy
/// button. `Some(entity)` means "the *next* click on that same entity's
/// destroy button actually destroys it"; anything else (a different
/// entity selected, or nothing selected) reads as unarmed.
#[derive(Resource, Default)]
struct PendingDestroy(Option<Entity>);

/// Shown while any building *you own* is selected (click-to-select — see
/// `selection.rs`): a Land Factory gets its villager-queue controls (the
/// "click the Land Factory and queue up units" interaction — a Land
/// Factory used to spawn one villager automatically every interval with
/// no player input at all; now every spawn has to be queued here first,
/// see `server::villagers`'s `VillagerQueues`), and every kind gets a
/// destroy button. The destroy button requires two clicks — the first
/// just arms `PendingDestroy` and relabels itself as a confirmation, the
/// second (only while still armed on this *same* entity) actually sends
/// `DestroyBuildingMsg` — so a single accidental click can never demolish
/// something.
#[allow(clippy::too_many_arguments)]
fn draw_selected_building_panel(
    mut contexts: EguiContexts,
    mut commands: Commands,
    menu_open: Res<crate::pilot::MenuOpen>,
    selected: Res<Selected>,
    local_player_id: Res<LocalPlayerId>,
    buildings: Query<&BuildingSnapshot>,
    local_queue_q: Query<&VillagerQueue, With<LocalPlayerAccount>>,
    villagers: Query<&VillagerSnapshot>,
    mut pending_destroy: ResMut<PendingDestroy>,
) -> Result {
    // Same "hidden unless `E`" gate `draw_build_bar` uses — part of the
    // same build UI.
    if !menu_open.0 {
        return Ok(());
    }
    let Some(entity) = selected.0 else {
        pending_destroy.0 = None;
        return Ok(());
    };
    let Ok(snapshot) = buildings.get(entity) else {
        pending_destroy.0 = None;
        return Ok(());
    };
    // Selecting anything else de-arms a previously-armed confirmation —
    // otherwise a click that merely re-selects a *different* building
    // right after arming this one's destroy button could go on to detonate
    // whichever one happens to still read as "armed" on the next click.
    if pending_destroy.0.is_some_and(|armed| armed != entity) {
        pending_destroy.0 = None;
    }
    let Some(player_id) = local_player_id.0 else { return Ok(()) };
    if snapshot.owner_player_id != player_id {
        // Someone else's building — nothing for you to control here.
        return Ok(());
    }

    egui::Window::new(format!("{:?}", snapshot.kind))
        .id(egui::Id::new("selected_building_panel"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-14.0, -90.0))
        .show(contexts.ctx_mut()?, |ui| {
            if snapshot.kind == BuildingKind::LandFactory
                && let Ok(queue) = local_queue_q.single()
            {
                let alive = villagers.iter().filter(|v| v.owner_player_id == player_id).count() as u32;
                let total = alive + queue.queued;
                let at_cap = total >= MAX_VILLAGERS_PER_PLAYER;

                ui.label(format!("Villagers: {alive} alive, {} queued", queue.queued));
                ui.label(format!("Cap: {total}/{MAX_VILLAGERS_PER_PLAYER}"));
                ui.add_space(6.0);
                ui.add_enabled_ui(!at_cap, |ui| {
                    // `Sense::CLICK`, not the default (focusable) sense
                    // `ui.button` would use — see `building_button`'s own
                    // docs on why every clickable in this UI is mouse-only.
                    if ui.add(egui::Button::new("Queue Villager").sense(egui::Sense::CLICK)).clicked() {
                        commands.client_trigger(QueueVillagerMsg);
                    }
                });
                if at_cap {
                    ui.label(egui::RichText::new("At the villager cap").weak());
                }
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);
            }

            let armed = pending_destroy.0 == Some(entity);
            let label = if armed { "⚠ Click again to confirm" } else { "🗑 Destroy" };
            let fill = if armed {
                egui::Color32::from_rgb(200, 45, 45)
            } else {
                egui::Color32::from_rgb(90, 35, 35)
            };
            if ui.add(egui::Button::new(label).fill(fill).sense(egui::Sense::CLICK)).clicked() {
                if armed {
                    commands.client_trigger(DestroyBuildingMsg { building_id: snapshot.id });
                    pending_destroy.0 = None;
                } else {
                    pending_destroy.0 = Some(entity);
                }
            }
        });

    Ok(())
}
