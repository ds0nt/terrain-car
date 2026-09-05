use bevy::camera::{Exposure, Hdr};
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::light::AtmosphereEnvironmentMapLight;
use bevy::pbr::AtmosphereSettings;
use bevy::post_process::bloom::Bloom;
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;

use crate::car::{CarChassis, CarResetEvent, LocalCar};

/// How far in front of whatever the camera would clip into to hold it —
/// enough that terrain right at the lens doesn't poke through into view.
const CAMERA_CLIP_MARGIN: f32 = 0.4;
/// Never pull the chase camera closer than this even if something is right
/// behind the car — a camera glued to the bumper is worse than brief clipping.
const CAMERA_MIN_DISTANCE: f32 = 1.5;

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CameraMode>()
            .add_systems(Startup, spawn_camera)
            .add_systems(Update, (toggle_camera_mode, update_camera).chain());
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

fn toggle_camera_mode(keyboard: Res<ButtonInput<KeyCode>>, mut mode: ResMut<CameraMode>) {
    if keyboard.just_pressed(KeyCode::KeyC) {
        *mode = match *mode {
            CameraMode::Chase => CameraMode::Cockpit,
            CameraMode::Cockpit => CameraMode::Chase,
        };
    }
}

fn update_camera(
    time: Res<Time>,
    mode: Res<CameraMode>,
    rapier_context: ReadRapierContext,
    chassis_q: Query<(Entity, &GlobalTransform, &CarChassis), With<LocalCar>>,
    mut camera_q: Query<&mut Transform, With<CarCamera>>,
    mut reset_events: MessageReader<CarResetEvent>,
) {
    let Ok((chassis_entity, chassis_gt, chassis)) = chassis_q.single() else {
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

    match *mode {
        CameraMode::Chase => {
            let look_target = chassis_transform.translation + Vec3::Y * 1.0;
            let mut desired = chassis_transform.translation - *chassis_transform.forward() * 9.0
                + Vec3::Y * 4.0;

            // Terrain here can be steep enough that a fixed chase distance
            // regularly ends up inside a cliff face — pull the camera in
            // front of whatever it would clip into instead.
            if let Ok(context) = rapier_context.single() {
                let offset = desired - look_target;
                let distance = offset.length();
                if distance > 0.01
                    && let Some((_, toi)) = context.cast_ray(
                        look_target,
                        offset / distance,
                        distance,
                        true,
                        QueryFilter::new().exclude_rigid_body(chassis_entity),
                    )
                {
                    let clamped = (toi - CAMERA_CLIP_MARGIN).max(CAMERA_MIN_DISTANCE);
                    desired = look_target + (offset / distance) * clamped;
                }
            }

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
