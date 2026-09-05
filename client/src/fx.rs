use bevy::prelude::*;

/// Reusable primitives for short-lived cosmetic effects (flashes, shells,
/// tracers, ...) — extracted from what `lightning_fx.rs` originally did as
/// one bespoke component pair (`LightningFlash`/`LightningFlashLight`) with
/// its own hand-written age/fade/despawn bookkeeping. Every future effect
/// (gun muzzle flash, impact flash, tracer) composes these instead of
/// reinventing that bookkeeping per effect.
pub struct FxPlugin;

impl Plugin for FxPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (fade_out, grow_scale, fade_light, tick_lifetimes));
    }
}

/// Generic self-despawning timer: attach to any effect entity that should
/// simply cease to exist after `duration` seconds. Other systems (below, or
/// effect-specific ones) read `progress()` to animate themselves over the
/// same span rather than tracking their own age.
#[derive(Component)]
pub struct Lifetime {
    age: f32,
    duration: f32,
}

impl Lifetime {
    pub fn new(duration: f32) -> Self {
        Self { age: 0.0, duration }
    }

    /// 0.0 at spawn, 1.0 at (or past) the end of its lifetime.
    pub fn progress(&self) -> f32 {
        if self.duration <= 0.0 {
            1.0
        } else {
            (self.age / self.duration).clamp(0.0, 1.0)
        }
    }
}

fn tick_lifetimes(time: Res<Time>, mut commands: Commands, mut q: Query<(Entity, &mut Lifetime)>) {
    let dt = time.delta_secs();
    for (entity, mut lifetime) in &mut q {
        lifetime.age += dt;
        if lifetime.age >= lifetime.duration {
            commands.entity(entity).despawn();
        }
    }
}

/// Opt-in: fades a `StandardMaterial`'s alpha and emissive down to zero
/// over the entity's `Lifetime`, for effects that should visibly dim away
/// (flashes, impacts) rather than just popping out of existence at
/// despawn.
#[derive(Component)]
pub struct FadeOut {
    pub base_alpha: f32,
    pub base_emissive: LinearRgba,
}

fn fade_out(
    mut materials: ResMut<Assets<StandardMaterial>>,
    q: Query<(&Lifetime, &FadeOut, &MeshMaterial3d<StandardMaterial>)>,
) {
    for (lifetime, fade, material) in &q {
        let Some(mut mat) = materials.get_mut(&material.0) else {
            continue;
        };
        let remaining = 1.0 - lifetime.progress();
        mat.base_color.set_alpha(fade.base_alpha * remaining);
        mat.emissive = fade.base_emissive * remaining;
    }
}

/// Opt-in: scales an entity from `start_scale` to `end_scale` over its
/// `Lifetime`, easing fast-then-slow (sqrt of progress) — a shockwave
/// expanding outward, not a linear balloon. Used by the lightning flash and
/// (later) gun impact flashes.
#[derive(Component)]
pub struct GrowScale {
    pub start_scale: f32,
    pub end_scale: f32,
}

fn grow_scale(mut q: Query<(&Lifetime, &GrowScale, &mut Transform)>) {
    for (lifetime, grow, mut transform) in &mut q {
        let t = lifetime.progress().sqrt();
        let scale = grow.start_scale + (grow.end_scale - grow.start_scale) * t;
        transform.scale = Vec3::splat(scale);
    }
}

/// Opt-in: fades a `PointLight`'s intensity down to zero over the entity's
/// `Lifetime` — the flash-of-light half of an explosion/muzzle flash,
/// separate from `FadeOut` since a light has no material to fade.
#[derive(Component)]
pub struct FadeLight {
    pub base_intensity: f32,
}

fn fade_light(mut q: Query<(&Lifetime, &FadeLight, &mut PointLight)>) {
    for (lifetime, fade, mut light) in &mut q {
        light.intensity = fade.base_intensity * (1.0 - lifetime.progress());
    }
}
