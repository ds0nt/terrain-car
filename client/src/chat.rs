use std::collections::VecDeque;

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPrimaryContextPass};
use bevy_replicon::prelude::ClientTriggerExt;
use shared::protocol::{ChatBroadcastMsg, ChatMsg};

use crate::auth_ui::AuthState;
use crate::owner_color::{color_for_owner, to_egui_color32};

/// In-game chat: `Enter` opens a bottom-left input box, `Enter` again sends
/// it, `Escape` cancels. Also this game's command line — anything typed
/// starting with `/` is parsed server-side rather than broadcast as a
/// literal line (`/tp <player> <player>`, op only, and `/respawn` — see
/// `server::chat`), the same "client only ever sends intent, server
/// decides" trust boundary every other action in this game already uses.
pub struct ChatPlugin;

impl Plugin for ChatPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChatOpen>()
            .init_resource::<ChatDraft>()
            .init_resource::<ChatFocusPending>()
            .init_resource::<ChatLog>()
            .add_systems(Update, open_chat_on_enter)
            .add_systems(EguiPrimaryContextPass, draw_chat)
            .add_observer(on_chat_broadcast);
    }
}

fn chat_text_edit_id() -> egui::Id {
    egui::Id::new("chat_text_edit")
}

/// Whether the chat input box currently has keyboard focus. Every other
/// system in this game that reads raw `ButtonInput<KeyCode>` for a
/// gameplay action (driving, walking, firing, ping, the vehicle-recall/
/// menu/reset/regen/cosmetics/recording/camera-toggle hotkeys...) gates on
/// this and does nothing while it's true, the same way each already gates
/// on `ControlMode`. Necessary because `bevy_egui` claims keystrokes for
/// its own focused text box but doesn't stop Bevy's own
/// `ButtonInput<KeyCode>` from *also* seeing every one of them — without
/// this, typing an ordinary sentence into chat would also drive the car,
/// fire the gun, and recall it to a Hangar, one letter at a time.
#[derive(Resource, Default)]
pub struct ChatOpen(pub bool);

/// A `run_if` condition for gating a system off entirely while chat is
/// open, for the (rare) case where adding a plain `Res<ChatOpen>`
/// parameter to that system directly would push it over Bevy's per-system
/// parameter-count limit (see `pilot::handle_vehicle_key`, the one system
/// in this game that's actually that close to it). Prefer the plain
/// parameter + inline check everywhere else — this loses the ability to
/// do any cleanup work (draining a buffered `MessageReader`, actively
/// zeroing stale input) on the gated frames, since the system doesn't run
/// at all.
pub fn chat_closed(chat_open: Res<ChatOpen>) -> bool {
    !chat_open.0
}

#[derive(Resource, Default)]
struct ChatDraft(String);

/// Set for exactly one frame right after chat opens — `draw_chat` consumes
/// it to call `request_focus` on the text box once, rather than fighting
/// the user for focus on every single frame it's open.
#[derive(Resource, Default)]
struct ChatFocusPending(bool);

struct ChatLine {
    text: String,
    color: egui::Color32,
    received_at: f32,
}

/// Ring buffer of recent chat lines, newest last — capped both by count and
/// (once the box itself is closed) by age, same "bounded, aging list" shape
/// `pings.rs`'s `RecentPings` already uses.
#[derive(Resource, Default)]
struct ChatLog(VecDeque<ChatLine>);

const MAX_LOG_LINES: usize = 50;
const LOG_VISIBLE_COUNT: usize = 10;
const LOG_FADE_SECS: f32 = 10.0;
const MAX_DRAFT_CHARS: usize = 240;
/// Reported live as too small to read comfortably at a glance — bumped
/// well past `egui`'s own default label size (~14px) for both the log and
/// the input box, alongside wider boxes to match (see `draw_chat`).
const CHAT_FONT_SIZE: f32 = 20.0;

fn open_chat_on_enter(
    keyboard: Res<ButtonInput<KeyCode>>,
    auth_state: Res<AuthState>,
    mut chat_open: ResMut<ChatOpen>,
    mut draft: ResMut<ChatDraft>,
    mut focus_pending: ResMut<ChatFocusPending>,
) {
    if chat_open.0 || !matches!(*auth_state, AuthState::LoggedIn) {
        return;
    }
    if keyboard.just_pressed(KeyCode::Enter) || keyboard.just_pressed(KeyCode::NumpadEnter) {
        chat_open.0 = true;
        draft.0.clear();
        focus_pending.0 = true;
    }
}

fn draw_chat(
    mut contexts: EguiContexts,
    mut chat_open: ResMut<ChatOpen>,
    mut draft: ResMut<ChatDraft>,
    mut focus_pending: ResMut<ChatFocusPending>,
    log: Res<ChatLog>,
    time: Res<Time>,
    mut commands: Commands,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    let now = time.elapsed_secs();
    // Sits just above the input box while it's open, otherwise pinned to
    // the same bottom-left inset the box itself uses when closed.
    let log_offset = egui::vec2(16.0, if chat_open.0 { -64.0 } else { -16.0 });

    egui::Area::new(egui::Id::new("chat_log")).anchor(egui::Align2::LEFT_BOTTOM, log_offset).show(ctx, |ui| {
        ui.set_max_width(680.0);
        let visible: Vec<_> = log
            .0
            .iter()
            .rev()
            .take(LOG_VISIBLE_COUNT)
            .filter(|line| chat_open.0 || now - line.received_at < LOG_FADE_SECS)
            .collect();
        for line in visible.into_iter().rev() {
            ui.label(egui::RichText::new(&line.text).size(CHAT_FONT_SIZE).color(line.color));
        }
    });

    if chat_open.0 {
        egui::Area::new(egui::Id::new("chat_box")).anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(16.0, -16.0)).show(
            ctx,
            |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(620.0);
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut draft.0)
                            .id(chat_text_edit_id())
                            .font(egui::FontId::proportional(CHAT_FONT_SIZE))
                            .hint_text("Enter to send, /tp <player> <player>, /respawn — Esc to cancel"),
                    );
                    // The very `Enter` press that opened the box this frame
                    // (`open_chat_on_enter`) is still sitting in egui's own
                    // input for this same frame — checking for a submit/
                    // cancel key unconditionally below would immediately
                    // see that identical keypress and close the box on the
                    // spot, same frame it opened, before anyone could type
                    // anything (reported live as "Enter does not open the
                    // chat" — it did open, for one imperceptible frame).
                    // `focus_pending` is true on exactly that one frame, so
                    // skipping the submit/cancel check while it's set is
                    // enough to consume that keypress here without also
                    // needing bevy's own `ButtonInput` cleared or anything
                    // more involved.
                    let just_opened = focus_pending.0;
                    if focus_pending.0 {
                        response.request_focus();
                        focus_pending.0 = false;
                    }
                    if draft.0.chars().count() > MAX_DRAFT_CHARS {
                        draft.0 = draft.0.chars().take(MAX_DRAFT_CHARS).collect();
                    }
                    if !just_opened {
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
                        if enter {
                            let text = draft.0.trim().to_string();
                            if !text.is_empty() {
                                commands.client_trigger(ChatMsg { text });
                            }
                            draft.0.clear();
                            chat_open.0 = false;
                        } else if escape {
                            draft.0.clear();
                            chat_open.0 = false;
                        }
                    }
                });
            },
        );
    }

    Ok(())
}

/// Server -> every client (an ordinary line) or this client only (a
/// command's own feedback, or a heads-up that someone teleported you — see
/// `server::chat`) — either way, just append it to the log.
fn on_chat_broadcast(msg: On<ChatBroadcastMsg>, mut log: ResMut<ChatLog>, time: Res<Time>) {
    let (text, color) = match msg.player_id {
        Some(player_id) => (format!("{}: {}", msg.username, msg.text), to_egui_color32(color_for_owner(player_id))),
        None => (format!("[server] {}", msg.text), egui::Color32::from_rgb(255, 205, 90)),
    };
    log.0.push_back(ChatLine { text, color, received_at: time.elapsed_secs() });
    if log.0.len() > MAX_LOG_LINES {
        log.0.pop_front();
    }
}
