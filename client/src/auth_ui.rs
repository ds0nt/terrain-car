use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPlugin, EguiPrimaryContextPass};
use bevy_replicon::prelude::{ClientState, ClientTriggerExt};
use shared::auth::{validate_password, validate_username};
use shared::protocol::{AuthResultMsg, LoginMsg, RegisterMsg};
use uuid::Uuid;

/// Real accounts, replacing the old "trust whatever UUID the client
/// claims" identity — see `shared::protocol::RegisterMsg`/`LoginMsg`'s
/// docs. Nothing is drivable until `AuthState` reaches `LoggedIn` — the
/// client's own locally-predicted car (see `car.rs`'s
/// `spawn_car_after_login`) doesn't spawn until then either, not just the
/// server-authoritative one; only the world itself (terrain, camera) loads
/// in the background while the login window is up.
///
/// Registers `bevy_egui::EguiPlugin` for the whole app — this used to be
/// `tuning_ui.rs`'s job, but that panel was removed (client-side car
/// tuning let any connected player self-serve a stats advantage); this is
/// now the earliest/most fundamental egui consumer, so every other egui
/// module (`building_ui.rs`, `minimap.rs`, `players_ui.rs`) leaves the
/// registration to this one. Bevy panics on a duplicate registration.
pub struct AuthUiPlugin;

impl Plugin for AuthUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin::default())
            .init_resource::<AuthState>()
            .init_resource::<LoginForm>()
            .add_observer(apply_auth_result)
            .add_systems(EguiPrimaryContextPass, draw_login_window);
    }
}

/// Where the last-used username is cached purely to prefill the login
/// form on relaunch — never the password, and never trusted for anything;
/// the server is the sole authority on identity every single login.
const LAST_USERNAME_PATH: &str = "last_username.txt";

#[derive(Resource, Default, Clone, Copy)]
pub enum AuthState {
    #[default]
    LoggedOut,
    Pending,
    /// Carries the account's durable `player_id` — needed by `car.rs`'s
    /// `spawn_car_after_login` to derive a stable, account-based paint
    /// color (see `owner_color::seed_from_uuid`) instead of the old
    /// per-connection one.
    LoggedIn(Uuid),
}

#[derive(Resource)]
struct LoginForm {
    username: String,
    password: String,
    error: Option<String>,
}

impl Default for LoginForm {
    fn default() -> Self {
        let username = std::fs::read_to_string(LAST_USERNAME_PATH).unwrap_or_default();
        Self { username: username.trim().to_string(), password: String::new(), error: None }
    }
}

fn draw_login_window(
    mut contexts: EguiContexts,
    mut state: ResMut<AuthState>,
    mut form: ResMut<LoginForm>,
    mut commands: Commands,
    client_state: Res<State<ClientState>>,
) -> Result {
    if !matches!(*state, AuthState::LoggedOut | AuthState::Pending) {
        return Ok(());
    }

    // Nothing to log into yet — the transport connection itself (see
    // `net.rs`'s `connect_to_server`) hasn't finished. Shown as its own
    // step rather than just a disabled form so it's obvious *what's*
    // being waited on if the server happens to be unreachable.
    if *client_state.get() != ClientState::Connected {
        egui::Window::new("Log In").collapsible(false).resizable(false).show(contexts.ctx_mut()?, |ui| {
            ui.label("Connecting to server...");
        });
        return Ok(());
    }

    let pending = matches!(*state, AuthState::Pending);

    egui::Window::new("Log In").collapsible(false).resizable(false).show(contexts.ctx_mut()?, |ui| {
        ui.add_enabled_ui(!pending, |ui| {
            ui.horizontal(|ui| {
                ui.label("Username:");
                ui.text_edit_singleline(&mut form.username);
            });
            ui.horizontal(|ui| {
                ui.label("Password:");
                ui.add(egui::TextEdit::singleline(&mut form.password).password(true));
            });

            ui.horizontal(|ui| {
                if ui.button("Log In").clicked() {
                    try_submit(&mut state, &mut form, &mut commands, false);
                }
                if ui.button("Register").clicked() {
                    try_submit(&mut state, &mut form, &mut commands, true);
                }
            });
        });

        if pending {
            ui.label("...");
        }
        if let Some(error) = &form.error {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), error);
        }
    });

    Ok(())
}

fn try_submit(state: &mut AuthState, form: &mut LoginForm, commands: &mut Commands, is_register: bool) {
    if let Err(reason) = validate_username(&form.username) {
        form.error = Some(reason.to_string());
        return;
    }
    if let Err(reason) = validate_password(&form.password) {
        form.error = Some(reason.to_string());
        return;
    }
    form.error = None;
    let _ = std::fs::write(LAST_USERNAME_PATH, &form.username);
    if is_register {
        commands.client_trigger(RegisterMsg {
            username: form.username.clone(),
            password: form.password.clone(),
        });
    } else {
        commands
            .client_trigger(LoginMsg { username: form.username.clone(), password: form.password.clone() });
    }
    *state = AuthState::Pending;
}

fn apply_auth_result(result: On<AuthResultMsg>, mut state: ResMut<AuthState>, mut form: ResMut<LoginForm>) {
    if result.ok {
        let player_id = result.player_id.expect("AuthResultMsg with ok=true always carries a player_id");
        info!("client: logged in as `{player_id}`");
        *state = AuthState::LoggedIn(player_id);
        form.password.clear();
    } else {
        warn!("client: auth failed: {}", result.message);
        form.error = Some(result.message.clone());
        *state = AuthState::LoggedOut;
    }
}
