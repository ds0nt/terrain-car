use bevy::input::mouse::MouseMotion;
use bevy::math::DVec3;
use bevy::prelude::*;
use bevy_replicon::prelude::ClientTriggerExt;
use shared::protocol::{PlaneInputMsg, PlaneSnapshot};
use uuid::Uuid;

use crate::auth_ui::LocalPlayerId;
use crate::camera::{CameraMode, CarCamera};
use crate::owner_color::color_for_owner;
use crate::pilot::ControlMode;
use crate::thrusters::{spawn_thrusters, sync_thruster_glow, ThrusterAxis, ThrusterMount};
use crate::worldspace::WorldOrigin;

/// Scout Plane rendering/flight — see `shared::protocol::PlaneSnapshot`'s
/// docs for why a plane, unlike the local car, is driven purely by its
/// replicated snapshot with no client-side prediction, and
/// `BuildingKind::AirFactory`'s for how one actually gets produced. The
/// actual exit-car/walk-over/enter-plane interaction lives in
/// `pilot.rs`, which owns `ControlMode` — this module just reacts to it
/// (flying only while `ControlMode::Plane`, same as everything else that
/// checks it).
pub struct AircraftPlugin;

impl Plugin for AircraftPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlaneInput>()
            .init_resource::<DrivingPlaneId>()
            .init_resource::<PlaneCameraMode>()
            .add_observer(init_plane_visuals)
            .add_systems(
                Update,
                (
                    tag_local_plane,
                    reset_plane_stick_on_enter,
                    read_plane_input,
                    sync_plane_transforms,
                    update_plane_camera,
                    sync_plane_thrusters,
                )
                    .chain(),
            )
            .add_systems(FixedUpdate, send_plane_input);
    }
}

/// Marks the one replicated `PlaneSnapshot` entity that belongs to the
/// local player, if any — set once by `tag_local_plane` the moment a
/// plane's ownership can be checked against `LocalPlayerId`, so every
/// other system here (camera follow, input routing) can just query for
/// this instead of re-comparing UUIDs every frame.
#[derive(Component)]
pub struct LocalPlane;

/// Which specific owned plane (`PlaneSnapshot::plane_id`) the player is
/// actually flying right now — `None` whenever `ControlMode` isn't
/// `Plane`. Set by `pilot::handle_vehicle_key` on boarding (the *nearest*
/// plane within `ENTER_RADIUS`, not just any owned one) and cleared on
/// exit — same shape and same reason as `car::DrivingCarId`: a multi-plane
/// owner shares one `owner_player_id` across all of them, so without this
/// the camera, `send_plane_input`, and recall all used to just grab
/// whichever owned plane happened to be first, reported live as boarding
/// one plane appearing to fly/move all of them.
#[derive(Resource, Default, Clone, Copy)]
pub struct DrivingPlaneId(pub Option<Uuid>);

/// A plane's own camera mode — deliberately a separate resource from the
/// car's `CameraMode` (not the same shared one), defaulting to `Cockpit`
/// instead of that one's `Chase`: flying first-person by default was asked
/// for specifically for planes, and forcing the *shared* resource to that
/// default would have also flipped every car's default view the moment
/// this shipped. `C` (`camera::toggle_camera_mode`) still toggles between
/// the two, same key, it just picks which of the two resources to flip
/// based on `ControlMode` at the moment it's pressed.
#[derive(Resource, Clone, Copy)]
pub struct PlaneCameraMode(pub CameraMode);

impl Default for PlaneCameraMode {
    fn default() -> Self {
        PlaneCameraMode(CameraMode::Cockpit)
    }
}

/// `pub` (and every field `pub`) so `hud.rs`'s stick-position indicator can
/// read `pitch`/`roll` directly — the crosshair alone gives no sense of
/// *how far* the mouse-driven virtual stick is currently pushed (see that
/// module's own docs), and this is the actual live state that answers it.
#[derive(Resource, Default)]
pub struct PlaneInput {
    pub throttle: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
}

/// Radians per pixel of raw mouse motion, mapped to pitch/roll — same
/// order of magnitude as `pilot.rs`'s own `MOUSE_SENSITIVITY` and
/// `camera.rs`'s `ORBIT_SENSITIVITY`, tuned separately since this is
/// scaled against a `[-1, 1]` stick input rather than a direct rotation.
/// Lowered from an initial `0.006` — reported live as too twitchy, small
/// mouse moves were swinging the stick too far.
const PLANE_MOUSE_SENSITIVITY: f32 = 0.0035;
/// Below this many raw pixels of motion in a single frame, treat mouse
/// movement as sensor/OS noise and ignore it entirely. Matters more here
/// than an ordinary look-around camera: the stick *accumulates* motion
/// rather than snapping to an instantaneous position (see
/// `read_plane_input`'s own docs on why), so without a deadzone even tiny
/// per-frame jitter while the mouse is physically at rest would slowly
/// but steadily drift pitch/roll away from center.
const PLANE_MOUSE_DEADZONE_PX: f32 = 0.8;
/// Exponential smoothing rate for `sync_plane_transforms` — matches
/// `camera.rs`'s own Cockpit-mode rate (also 20.0), which was tuned for
/// "smooths out real jitter without reading as a lagging chase cam"; the
/// same balance applies here.
const SMOOTHING_RATE: f32 = 20.0;

/// Tags a newly-replicated plane as `LocalPlane` once its ownership can be
/// checked. A retried `Update` system, not a one-shot `On<Insert,
/// PlaneSnapshot>` observer: `PlaneSnapshot` replication and the
/// `AuthResultMsg` this client's own `LocalPlayerId` comes from travel
/// over separate renet channels with no ordering guarantee, so an
/// insert-time-only check can silently lose that race and never tag the
/// plane at all (see `player_account.rs`'s `tag_local_player_account`,
/// which hit exactly this live — same fix applied here and to
/// `car.rs`'s `tag_local_car` for the same reason, even though a plane
/// only ever appears well after login, by which point the race is far
/// less likely to actually bite). `Without<LocalPlane>` keeps the retry
/// cheap (a tagged plane, or someone else's, is skipped every frame).
fn tag_local_plane(
    mut commands: Commands,
    local_player_id: Res<LocalPlayerId>,
    planes: Query<(Entity, &PlaneSnapshot), Without<LocalPlane>>,
) {
    let Some(player_id) = local_player_id.0 else {
        return;
    };
    for (entity, snapshot) in &planes {
        if snapshot.owner_player_id == player_id {
            commands.entity(entity).insert(LocalPlane);
        }
    }
}

/// Simple procedural placeholder mesh (no external texture/model assets,
/// matching this project's existing cosmetic style) — a fuselage box,
/// symmetric wings, and a tail fin, tinted by the owner's identity color
/// the same way a building's body is (see `owner_color::color_for_owner`).
fn init_plane_visuals(
    insert: On<Insert, PlaneSnapshot>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    flame_effect: Res<crate::thrusters::ThrusterFlameEffect>,
    planes: Query<&PlaneSnapshot>,
) {
    let Ok(snapshot) = planes.get(insert.entity) else {
        return;
    };
    let color = color_for_owner(snapshot.owner_player_id);
    let material = materials.add(StandardMaterial { base_color: color, perceptual_roughness: 0.4, ..default() });

    // `Transform::IDENTITY`, not a spawn-time rotation: `sync_plane_transforms`
    // overwrites this entity's `rotation` from the replicated snapshot every
    // single frame (same "no local prediction" shape `car_render.rs`'s
    // `init_car_visuals` docs already explain for a car), so anything set
    // here would be stomped on the very next frame and have zero visible
    // effect — a previous fix tried exactly that (rotating the whole plane
    // 180° at spawn) and, predictably, changed nothing. The real bug was
    // the tail fin child below sitting at local `Z = -1.5`: Bevy's forward
    // is `-Z`, so that placed the tail at the *nose* — fixed by moving it
    // to `+Z` (the actual back) instead.
    commands.entity(insert.entity).insert((
        Mesh3d(meshes.add(Cuboid::new(1.6, 0.7, 3.2))),
        MeshMaterial3d(material.clone()),
        Transform::IDENTITY,
        Visibility::default(),
    ));
    commands.entity(insert.entity).with_children(|parent| {
        // Wings: one wide, thin box spanning both sides.
        parent.spawn((
            Mesh3d(meshes.add(Cuboid::new(6.0, 0.15, 1.1))),
            MeshMaterial3d(material.clone()),
            Transform::from_xyz(0.0, 0.0, 0.1),
        ));
        // Tail fin: a small vertical box at the back (+Z — see docs above).
        parent.spawn((
            Mesh3d(meshes.add(Cuboid::new(0.15, 1.0, 0.8))),
            MeshMaterial3d(material),
            Transform::from_xyz(0.0, 0.5, 1.5),
        ));

        // Directional thruster nozzles — see `thrusters.rs`'s own module
        // docs for why every "which way am I steering" axis gets its own
        // pair, mounted roughly where a real RCS cluster would sit: main
        // engine at the tail, reverse/yaw at the nose, pitch top/bottom of
        // the tail, roll out at the wingtips.
        spawn_thrusters(
            parent,
            &mut meshes,
            &mut materials,
            &flame_effect.0,
            &[
                ThrusterMount { offset: Vec3::new(0.0, 0.0, 1.75), axis: ThrusterAxis::ThrottleForward },
                ThrusterMount { offset: Vec3::new(0.0, 0.0, -1.75), axis: ThrusterAxis::ThrottleReverse },
                ThrusterMount { offset: Vec3::new(0.6, 0.0, -1.5), axis: ThrusterAxis::YawPositive },
                ThrusterMount { offset: Vec3::new(-0.6, 0.0, -1.5), axis: ThrusterAxis::YawNegative },
                ThrusterMount { offset: Vec3::new(0.0, 0.4, 1.4), axis: ThrusterAxis::PitchPositive },
                ThrusterMount { offset: Vec3::new(0.0, -0.4, 1.4), axis: ThrusterAxis::PitchNegative },
                ThrusterMount { offset: Vec3::new(2.9, 0.0, 0.1), axis: ThrusterAxis::RollPositive },
                ThrusterMount { offset: Vec3::new(-2.9, 0.0, 0.1), axis: ThrusterAxis::RollNegative },
            ],
        );
    });
}

/// Eases every replicated plane's `Transform` toward its latest snapshot
/// every frame — the same "no prediction, just follow the snapshot" shape
/// `car_render.rs`'s `sync_car_transforms` uses for a car, just applied
/// here even to a plane *you* own (see `PlaneSnapshot`'s docs on why).
///
/// Smoothed (lerp/slerp), not a direct snap-to-latest: replication updates
/// arrive at the server's own tick rate, which rarely lines up evenly with
/// the client's render framerate, so a direct assignment holds the exact
/// same `Transform` for however many extra render frames pass between two
/// snapshots, then jumps discretely the instant the next one lands — the
/// choppiness reported live ("things get a bit choppy... server
/// corrections"). Easing toward the target every frame instead spreads
/// that same motion out smoothly across every render frame, snapshot or
/// not. `SMOOTHING_RATE` is fast enough that this reads as "smooth,"
/// converging within a couple of frames of each update, not "laggy" —
/// there's already some inherent input-to-effect latency in a purely
/// server-authoritative vehicle (see `PlaneSnapshot`'s docs), and this
/// doesn't meaningfully add to it.
fn sync_plane_transforms(
    time: Res<Time>,
    origin: Res<WorldOrigin>,
    mut planes: Query<(&PlaneSnapshot, &mut Transform)>,
) {
    let lerp_factor = 1.0 - (-SMOOTHING_RATE * time.delta_secs()).exp();
    for (snapshot, mut transform) in &mut planes {
        let local = (DVec3::new(snapshot.true_x, 0.0, snapshot.true_z) - origin.offset).as_vec3();
        let target = Vec3::new(local.x, snapshot.altitude, local.z);
        transform.translation = transform.translation.lerp(target, lerp_factor);
        transform.rotation = transform.rotation.slerp(snapshot.rotation, lerp_factor);
    }
}

/// Re-centers the flight stick the moment you start flying — see
/// `pilot::manage_cursor_confinement` for the actual cursor handling
/// (shared across `Plane` and `OnFoot`, not this module's concern
/// anymore). Without this, a stick left deflected at the end of a
/// previous flight would still read as pushed the instant you board again.
fn reset_plane_stick_on_enter(mode: Res<ControlMode>, mut input: ResMut<PlaneInput>) {
    if mode.is_changed() && *mode == ControlMode::Plane {
        input.pitch = 0.0;
        input.roll = 0.0;
    }
}

/// Throttle stays a plain key (W/S) and yaw stays a plain key too (A/D and
/// the left/right arrows — rudder), but pitch/roll are a mouse-driven
/// flight stick: raw `MouseMotion` delta this frame *accumulates* into the
/// current stick position (clamped to `[-1, 1]` per axis) instead of
/// replacing it — push the mouse right and stop moving, and roll stays
/// held at wherever you pushed it to, the same way a real joystick stays
/// deflected until you physically move it back toward center, rather than
/// snapping back to level the instant your hand stops. That only works
/// because `manage_plane_cursor` locks the cursor while flying: with an
/// unlocked cursor there's no "back toward center" to move *to* once it's
/// pinned against a screen edge. No strafe — a plane only ever moves along
/// its own forward axis.
fn read_plane_input(
    mode: Res<ControlMode>,
    menu_open: Res<crate::pilot::MenuOpen>,
    chat_open: Res<crate::chat::ChatOpen>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut mouse_motion: MessageReader<MouseMotion>,
    mut input: ResMut<PlaneInput>,
) {
    if *mode != ControlMode::Plane || chat_open.0 {
        mouse_motion.clear();
        if chat_open.0 {
            // Same "actively neutralize, don't just stop reading" reasoning
            // `car::read_car_input` uses — a plane keeps re-applying
            // whatever throttle it last had forever (see `send_plane_input`),
            // so leaving this untouched would let it keep flying itself
            // while you type.
            *input = PlaneInput::default();
        }
        return;
    }
    let mut throttle = 0.0;
    let mut yaw = 0.0;
    if keyboard.pressed(KeyCode::KeyW) {
        throttle += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyS) {
        throttle -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyA) || keyboard.pressed(KeyCode::ArrowLeft) {
        yaw += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyD) || keyboard.pressed(KeyCode::ArrowRight) {
        yaw -= 1.0;
    }
    input.throttle = throttle;
    input.yaw = yaw;

    // `MenuOpen` (`Alt`) hands the cursor back for clicking the build bar
    // (see `pilot::manage_cursor_confinement`) — moving it there shouldn't
    // also throw the plane into a roll out from under the player, so the
    // stick just holds wherever it already was, same as letting go of a
    // real one.
    if menu_open.0 {
        mouse_motion.clear();
        return;
    }
    let mut mouse_delta = Vec2::ZERO;
    for event in mouse_motion.read() {
        mouse_delta += event.delta;
    }
    // See `PLANE_MOUSE_DEADZONE_PX`'s own docs — below this, this frame's
    // motion is noise, not intent.
    if mouse_delta.x.abs() < PLANE_MOUSE_DEADZONE_PX {
        mouse_delta.x = 0.0;
    }
    if mouse_delta.y.abs() < PLANE_MOUSE_DEADZONE_PX {
        mouse_delta.y = 0.0;
    }
    // Minus, not plus — `MouseMotion::delta.y` is positive moving *down*
    // the screen, so subtracting it is what makes pushing the mouse up
    // pitch the nose up (a positive rotation about local +X, see
    // `server::aircraft::fly_planes`), matching the same non-inverted
    // convention `pilot.rs`'s on-foot camera pitch already uses
    // (`pitch.0 -= event.delta.y * ...`) — the plane had the sign flipped
    // from that, which is exactly what read as "inverted."
    input.pitch = (input.pitch - mouse_delta.y * PLANE_MOUSE_SENSITIVITY).clamp(-1.0, 1.0);
    input.roll = (input.roll - mouse_delta.x * PLANE_MOUSE_SENSITIVITY).clamp(-1.0, 1.0);
}

fn send_plane_input(
    mode: Res<ControlMode>,
    input: Res<PlaneInput>,
    driving: Res<DrivingPlaneId>,
    mut commands: Commands,
) {
    if *mode != ControlMode::Plane {
        return;
    }
    let Some(plane_id) = driving.0 else { return };
    commands.client_trigger(PlaneInputMsg {
        plane_id,
        throttle: input.throttle,
        yaw: input.yaw,
        pitch: input.pitch,
        roll: input.roll,
    });
}

/// Chase-or-cockpit camera for whichever plane `DrivingPlaneId` names,
/// active only while piloting one — `camera.rs`'s own `update_camera`
/// skips entirely in that state (see its `ControlMode` check) so the two
/// never fight over the same `CarCamera` transform in the same frame.
/// Which of the two: `PlaneCameraMode`, not the car's own `CameraMode` —
/// see that resource's own docs on why planes need a separate one
/// (defaults to `Cockpit`, first-person by default, as asked for).
pub fn update_plane_camera(
    time: Res<Time>,
    mode: Res<ControlMode>,
    camera_mode: Res<PlaneCameraMode>,
    driving: Res<DrivingPlaneId>,
    plane_q: Query<(&GlobalTransform, &PlaneSnapshot), With<LocalPlane>>,
    mut camera_q: Query<&mut Transform, With<CarCamera>>,
) {
    if *mode != ControlMode::Plane {
        return;
    }
    // Follows whichever plane `DrivingPlaneId` names (set by
    // `pilot::handle_vehicle_key` on boarding — the *nearest* one, not
    // just "first") — not `.iter().next()` (an arbitrary owned plane,
    // possibly parked somewhere else entirely) as this used to, which was
    // exactly the reported "boarding a plane moves all of them" bug the
    // moment anyone owned more than one.
    let Some(driving_id) = driving.0 else {
        return;
    };
    let Some((plane_gt, _)) = plane_q.iter().find(|(_, snapshot)| snapshot.plane_id == driving_id) else {
        return;
    };
    let Ok(mut camera_tf) = camera_q.single_mut() else {
        return;
    };

    let transform = plane_gt.compute_transform();
    // Plane-relative `up`, not world `Vec3::Y` — the plane can genuinely
    // bank and pitch now (see `PlaneInputMsg`'s docs), and a camera that
    // stays world-up-referenced while the aircraft rolls around it reads as
    // the *world* tilting around a fixed camera instead of the camera
    // banking along with the plane, which is backwards from how a chase
    // camera should feel. Using the plane's own up for both the offset and
    // the `looking_at` roll reference keeps the aircraft centered and
    // right-side-up in frame through a full roll, exactly like a real
    // flight-sim chase cam.
    let plane_up = *transform.up();
    let dt = time.delta_secs();

    let (desired_translation, desired_rotation, lerp_factor) = match camera_mode.0 {
        CameraMode::Chase => {
            let look_target = transform.translation + plane_up * 1.5;
            let desired = transform.translation - *transform.forward() * 12.0 + plane_up * 5.0;
            let rotation = Transform::from_translation(desired).looking_at(look_target, plane_up).rotation;
            // Bumped from `6.0` (the car chase cam's own rate) — reported
            // live as too draggy/laggy trailing the plane's own motion,
            // especially noticeable given how fast a plane covers ground
            // compared to a car.
            (desired, rotation, 1.0 - (-10.0 * dt).exp())
        }
        CameraMode::Cockpit => {
            // Just behind the nose, at roughly canopy height — see the
            // fuselage's own mesh dimensions in `init_plane_visuals`
            // (`Cuboid::new(1.6, 0.7, 3.2)`, forward is local `-Z`).
            // Inherits the plane's full rotation directly (no `looking_at`
            // needed) for the same reason the car's own cockpit view does:
            // "forward" here already *is* the plane's forward.
            let local_seat = Vec3::new(0.0, 0.25, -1.1);
            let desired = transform.transform_point(local_seat);
            // `1.0`, not a lerp rate — zero drag, rigidly bolted to the
            // plane every frame. Reported live as needing to be "really
            // crisp": even the snappy `20.0` rate used before this was
            // still a genuine one-or-more-frame lag behind a plane banking
            // hard, which reads as the camera sliding around inside the
            // cockpit instead of being part of the airframe. A real
            // cockpit view has no give at all — your head doesn't lag
            // behind the fuselage it's bolted to.
            (desired, transform.rotation, 1.0)
        }
    };

    camera_tf.translation = camera_tf.translation.lerp(desired_translation, lerp_factor);
    camera_tf.rotation = camera_tf.rotation.slerp(desired_rotation, lerp_factor);
}

/// Lights the piloted plane's own thruster nozzles from the live
/// `PlaneInput` stick — see `thrusters.rs`'s own module docs on why only
/// the plane you're actually flying ever lights up.
fn sync_plane_thrusters(
    mut materials: ResMut<Assets<StandardMaterial>>,
    input: Res<PlaneInput>,
    driving: Res<DrivingPlaneId>,
    planes: Query<(&PlaneSnapshot, &Children), With<LocalPlane>>,
    mut nozzles: crate::thrusters::NozzleQuery,
) {
    let Some(driving_id) = driving.0 else {
        return;
    };
    let Some((_, children)) = planes.iter().find(|(snapshot, _)| snapshot.plane_id == driving_id) else {
        return;
    };
    sync_thruster_glow(&mut materials, children, &mut nozzles, input.throttle, input.yaw, input.pitch, input.roll);
}
