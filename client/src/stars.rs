use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::camera::CarCamera;

/// A static starfield for the Mars-night sky (`lighting.rs`) — simple
/// procedural placeholder points (no external texture/model assets,
/// matching this project's existing cosmetic style): tiny unlit emissive
/// spheres scattered once, at Startup, on a huge sphere shell far past any
/// real gameplay draw distance, then kept centered on the camera every
/// frame (position only, never rotation) so they read as fixed background
/// regardless of where the player actually drives/flies/walks — the same
/// "skybox" trick real games use instead of literally modeling a
/// planetarium-scale sphere of stars around the whole map.
pub struct StarsPlugin;

impl Plugin for StarsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_stars).add_systems(Update, follow_camera);
    }
}

#[derive(Component)]
struct StarField;

const STAR_COUNT: usize = 500;
/// Comfortably past normal terrain streaming/draw distance (see
/// `terrain.rs`'s `LOAD_RADIUS_CHUNKS`), and within the camera's widened
/// far plane (`camera.rs`'s `spawn_camera`) — stars this far away never
/// visibly parallax as the player moves, which is exactly what sells "this
/// is the sky," not nearby geometry.
const STAR_SHELL_RADIUS: f32 = 4000.0;
const STAR_RADIUS: f32 = 4.0;
/// Fixed seed — the same starfield every run, not a new random sky each
/// launch (there's no reason for it to change, and a re-shuffled sky on
/// every restart would just read as a bug).
const STAR_SEED: u64 = 20260906;

fn spawn_stars(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>, mut materials: ResMut<Assets<StandardMaterial>>) {
    let mesh = meshes.add(Sphere::new(STAR_RADIUS));
    // A few brightness/tint variants so the sky reads as countless
    // individual stars, not one dot copy-pasted a few hundred times.
    let star_materials: Vec<_> = [
        Color::srgb(1.0, 1.0, 1.0),
        Color::srgb(0.8, 0.88, 1.0),
        Color::srgb(1.0, 0.9, 0.75),
        Color::srgb(0.9, 0.95, 1.0),
    ]
    .into_iter()
    .map(|color| {
        materials.add(StandardMaterial {
            base_color: color,
            emissive: LinearRgba::from(color) * 6.0,
            unlit: true,
            ..default()
        })
    })
    .collect();

    let mut rng = StdRng::seed_from_u64(STAR_SEED);
    commands
        .spawn((Transform::IDENTITY, Visibility::default(), StarField))
        .with_children(|parent| {
            for i in 0..STAR_COUNT {
                // Uniform point on a sphere shell: theta around the
                // equator, phi from an evenly-distributed cosine so points
                // don't bunch up at the poles.
                let theta = rng.gen_range(0.0..std::f32::consts::TAU);
                let phi = rng.gen_range(-1.0f32..1.0).acos();
                let direction =
                    Vec3::new(phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin());
                // Slight radius jitter so stars don't all sit at one exact
                // distance (a perfectly even shell reads as a "sphere,"
                // not a sky) — comfortably inside the camera's far plane.
                let radius = STAR_SHELL_RADIUS * rng.gen_range(0.8..1.0);

                parent.spawn((
                    Mesh3d(mesh.clone()),
                    MeshMaterial3d(star_materials[i % star_materials.len()].clone()),
                    Transform::from_translation(direction * radius),
                ));
            }
        });
}

/// Re-centers the starfield's root on the camera's current position every
/// frame — rotation is deliberately never touched, only translation, so
/// turning the camera doesn't spin the whole sky with it, only moving
/// (driving, flying, walking) does.
fn follow_camera(
    camera_q: Query<&GlobalTransform, With<CarCamera>>,
    mut star_q: Query<&mut Transform, With<StarField>>,
) {
    let Ok(camera_gt) = camera_q.single() else {
        return;
    };
    let Ok(mut star_tf) = star_q.single_mut() else {
        return;
    };
    star_tf.translation = camera_gt.translation();
}
