use bevy::camera::{Exposure, Hdr};
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::input::mouse::MouseMotion;
use bevy::light::AtmosphereEnvironmentMapLight;
use bevy::pbr::AtmosphereSettings;
use bevy::post_process::bloom::Bloom;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;

use crate::car::{CarChassis, CarResetEvent, DrivingCarId};
use crate::pilot::PassengerCarId;

/// How far in front of whatever the camera would clip into to hold it —
/// enough that terrain right at the lens doesn't poke through into view.
const CAMERA_CLIP_MARGIN: f32 = 0.4;
/// Never pull the chase camera closer than this even if something is right
/// behind the car — a camera glued to the bumper is worse than brief clipping.
const CAMERA_MIN_DISTANCE: f32 = 1.5;
/// How far the free-look camera orbits from — matches Chase's own distance
/// (the `9.0`/`4.0` in its `desired` computation) so looking around doesn't
/// also change how big the car reads on screen.
const ORBIT_DISTANCE: f32 = 9.8;
const ORBIT_SENSITIVITY: f32 = 0.006;
/// Just short of straight up/down — orbiting exactly to the pole would
/// otherwise let yaw spin freely with no visual reference (gimbal-lock
/// territory), which reads as the camera flipping.
const ORBIT_PITCH_LIMIT: f32 = 1.45;

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CameraMode>()
            .init_resource::<OrbitLook>()
            .add_systems(Startup, spawn_camera)
            .add_systems(Update, (toggle_camera_mode, handle_orbit_input, update_camera).chain());
    }
}

/// Free-look state: held while `MouseButton::Middle` is down (fully unused
/// anywhere else in this game — Left is claimed by selection/placement and
/// Right by placement-cancel, both state-gated, so Middle is the one
/// binding with zero chance of colliding with either). `yaw`/`pitch`
/// persist across holds rather than resetting each time, so releasing and
/// grabbing the view again doesn't snap back to "directly behind the car"
/// — it picks up roughly where you left it.
#[derive(Resource, Default)]
struct OrbitLook {
    active: bool,
    yaw: f32,
    pitch: f32,
}

fn handle_orbit_input(
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    mut motion: MessageReader<MouseMotion>,
    mut orbit: ResMut<OrbitLook>,
) {
    orbit.active = mouse_buttons.pressed(MouseButton::Middle);
    if !orbit.active {
        // Still drain the reader so a burst of motion while not orbiting
        // (e.g. moving the mouse before ever middle-clicking) doesn't
        // apply itself all at once the instant orbiting starts.
        motion.clear();
        return;
    }
    for event in motion.read() {
        orbit.yaw -= event.delta.x * ORBIT_SENSITIVITY;
        orbit.pitch = (orbit.pitch - event.delta.y * ORBIT_SENSITIVITY)
            .clamp(-ORBIT_PITCH_LIMIT, ORBIT_PITCH_LIMIT);
    }
}

#[derive(Resource, Default, PartialEq, Eq, Clone, Copy)]
pub enum CameraMode {
    #[default]
    Chase,
    Cockpit,
}

#[derive(Component)]
pub struct CarCamera;

fn spawn_camera(mut commands: Commands) {
    commands.spawn((
        Camera3d::default(),
        // Default far plane (1000.0) would clip `stars.rs`'s starfield,
        // which sits well past any real terrain/gameplay draw distance on
        // purpose (see that module's own docs) — nothing else in the
        // scene needs the extra range, so widening this costs nothing.
        Projection::Perspective(PerspectiveProjection { far: 8000.0, ..default() }),
        Transform::from_xyz(0.0, 5.0, -10.0).looking_at(Vec3::ZERO, Vec3::Y),
        CarCamera,
        Hdr,
        AtmosphereSettings::default(),
        // Raw sunlight (see lighting.rs) is very bright pre-scattering;
        // this brings the atmosphere-filtered result back to a normal
        // exposure range.
        Exposure { ev100: 13.0 },
        Tonemapping::AcesFitted,
        Bloom::NATURAL,
        // Lets the sky's actual color/brightness drive ambient light and
        // reflections instead of a flat fill light.
        AtmosphereEnvironmentMapLight::default(),
    ));
}

/// `C`: toggles Chase/Cockpit — but *which* of the two independent camera
/// modes (the car's own `CameraMode`, or the plane's separate
/// `PlaneCameraMode` — see that resource's own docs on why they aren't the
/// same one) depends on `ControlMode` at the moment it's pressed, so the
/// same key does the intuitive thing for whichever vehicle you're actually
/// in right now.
fn toggle_camera_mode(
    keyboard: Res<ButtonInput<KeyCode>>,
    chat_open: Res<crate::chat::ChatOpen>,
    control_mode: Res<crate::pilot::ControlMode>,
    mut mode: ResMut<CameraMode>,
    mut plane_mode: ResMut<crate::aircraft::PlaneCameraMode>,
) {
    if chat_open.0 || !keyboard.just_pressed(KeyCode::KeyC) {
        return;
    }
    let flip = |m: CameraMode| match m {
        CameraMode::Chase => CameraMode::Cockpit,
        CameraMode::Cockpit => CameraMode::Chase,
    };
    if *control_mode == crate::pilot::ControlMode::Plane {
        plane_mode.0 = flip(plane_mode.0);
    } else {
        *mode = flip(*mode);
    }
}

/// Pulls `desired` in front of whatever it would otherwise clip into,
/// looking back from `look_target` — shared by Chase and free-look, since
/// both are "camera sits some distance from a point, terrain might be in
/// the way" the same way.
fn avoid_clipping(
    rapier_context: &ReadRapierContext,
    exclude: Entity,
    look_target: Vec3,
    desired: Vec3,
) -> Vec3 {
    let Ok(context) = rapier_context.single() else {
        return desired;
    };
    let offset = desired - look_target;
    let distance = offset.length();
    if distance <= 0.01 {
        return desired;
    }
    let Some((_, toi)) = context.cast_ray(
        look_target,
        offset / distance,
        distance,
        true,
        QueryFilter::new().exclude_rigid_body(exclude),
    ) else {
        return desired;
    };
    let clamped = (toi - CAMERA_CLIP_MARGIN).max(CAMERA_MIN_DISTANCE);
    look_target + (offset / distance) * clamped
}

#[allow(clippy::too_many_arguments)]
fn update_camera(
    time: Res<Time>,
    mode: Res<CameraMode>,
    control_mode: Res<crate::pilot::ControlMode>,
    driving_car: Res<DrivingCarId>,
    passenger_car: Res<PassengerCarId>,
    orbit: Res<OrbitLook>,
    rapier_context: ReadRapierContext,
    // Not `With<LocalCar>`: a passenger's car is by definition someone
    // else's (see `PassengerCarId`'s own docs), so this has to be able to
    // find *any* car, not just an owned one — `target_car_id` below is
    // what actually narrows it down to the one right car.
    chassis_q: Query<(Entity, &GlobalTransform, &CarChassis)>,
    mut camera_q: Query<&mut Transform, With<CarCamera>>,
    mut reset_events: MessageReader<CarResetEvent>,
) {
    // While flying or on foot, `aircraft::update_plane_camera` or
    // `pilot::update_pilot_camera` drives this same camera instead — all
    // four mutually exclusive on `pilot::ControlMode` so they never fight
    // over the same `Transform` in one frame. A passenger gets the exact
    // same chase/cockpit view a driver would, just following someone
    // else's car — riding along with nothing to do but watch shouldn't
    // also mean staring at a fixed, un-chasing camera.
    let target_car_id = match *control_mode {
        crate::pilot::ControlMode::Car => driving_car.0,
        crate::pilot::ControlMode::Passenger => passenger_car.0,
        _ => return,
    };
    let Some(target_car_id) = target_car_id else {
        return;
    };
    let Some((chassis_entity, chassis_gt, chassis)) =
        chassis_q.iter().find(|(_, _, chassis)| chassis.car_id == target_car_id)
    else {
        return;
    };
    let Ok(mut camera_tf) = camera_q.single_mut() else {
        return;
    };

    let chassis_transform = chassis_gt.compute_transform();
    let dt = time.delta_secs();
    // A reset just teleported the car; snap instead of chasing it across
    // the map over the next second.
    let just_reset = reset_events.read().next().is_some();

    // Free-look overrides whichever `CameraMode` is active while the
    // button's held — it's a temporary "let me look around" gesture, not
    // a third persistent mode you toggle into. Releasing it just lets the
    // match below take over again next frame, lerping back into place
    // exactly like recovering from any other momentary camera excursion.
    if orbit.active {
        let look_target = chassis_transform.translation + Vec3::Y * 1.0;
        let orbit_rotation = Quat::from_euler(EulerRot::YXZ, orbit.yaw, orbit.pitch, 0.0);
        let desired = avoid_clipping(
            &rapier_context,
            chassis_entity,
            look_target,
            look_target + orbit_rotation * Vec3::new(0.0, 0.0, ORBIT_DISTANCE),
        );

        // Snappier than the chase lerp on purpose — a free-look camera
        // that laggily eases toward where you just aimed the mouse feels
        // unresponsive, not cinematic.
        let lerp_factor = if just_reset { 1.0 } else { 1.0 - (-16.0 * dt).exp() };
        camera_tf.translation = camera_tf.translation.lerp(desired, lerp_factor);
        let target_rotation =
            Transform::from_translation(camera_tf.translation).looking_at(look_target, Vec3::Y).rotation;
        camera_tf.rotation = camera_tf.rotation.slerp(target_rotation, lerp_factor);
        return;
    }

    match *mode {
        CameraMode::Chase => {
            let look_target = chassis_transform.translation + Vec3::Y * 1.0;
            let desired = chassis_transform.translation - *chassis_transform.forward() * 9.0
                + Vec3::Y * 4.0;

            // Terrain here can be steep enough that a fixed chase distance
            // regularly ends up inside a cliff face — pull the camera in
            // front of whatever it would clip into instead.
            let desired = avoid_clipping(&rapier_context, chassis_entity, look_target, desired);

            let lerp_factor = if just_reset { 1.0 } else { 1.0 - (-6.0 * dt).exp() };
            camera_tf.translation = camera_tf.translation.lerp(desired, lerp_factor);
            let target_rotation = Transform::from_translation(camera_tf.translation)
                .looking_at(look_target, Vec3::Y)
                .rotation;
            camera_tf.rotation = camera_tf.rotation.slerp(target_rotation, lerp_factor);
        }
        CameraMode::Cockpit => {
            // Inside the cabin bump (see spawn_car), just behind the
            // windshield. Car forward is local -Z, matching Bevy's own
            // camera-forward convention, so the cockpit camera can just
            // inherit the chassis rotation directly instead of building a
            // look_at.
            let local_seat = Vec3::new(
                0.0,
                chassis.half_extents.y * 1.7,
                -(chassis.half_extents.z * 0.1),
            );
            let desired_translation = chassis_transform.transform_point(local_seat);
            // A few degrees of upward look bias, so parking on a downslope
            // (or just normal suspension squat) doesn't point the default
            // view straight down at the hood.
            let desired_rotation =
                chassis_transform.rotation * Quat::from_rotation_x(-0.09);

            // Snappy but not infinitely stiff — smooths out physics-substep
            // jitter without feeling like a lagging chase cam.
            let lerp_factor = if just_reset { 1.0 } else { 1.0 - (-20.0 * dt).exp() };
            camera_tf.translation = camera_tf.translation.lerp(desired_translation, lerp_factor);
            camera_tf.rotation = camera_tf.rotation.slerp(desired_rotation, lerp_factor);
        }
    }
}
