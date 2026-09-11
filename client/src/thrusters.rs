use bevy::prelude::*;
use bevy_hanabi::prelude::*;

/// Shared directional-thruster visuals for the flying "ships" (plane,
/// dropship) — a small glowing, particle-spewing nozzle per control axis,
/// active whenever that axis is actually being commanded, the same idea a
/// spacecraft's RCS thruster cluster uses: not just "the engine is on,"
/// but "here's specifically what's pushing you which way right now."
/// Shared between `aircraft.rs` and `dropship.rs` (not the `Tank` — a
/// wheeled vehicle steers by turning its wheels, not by directional
/// thrust, so this concept doesn't apply there) for the same reason
/// `shared::tank_physics`/`turret_render.rs` share their own math/visuals
/// across vehicle kinds: this is genuinely the same mechanism in both
/// places, just mounted at different points for a different-shaped hull.
///
/// v1 scope: only ever lit/emitting for the local player's own actively-
/// piloted ship — the input a *remote* player is currently pushing isn't
/// replicated at all (only the physical outcome, via `PlaneSnapshot`/
/// `DropshipSnapshot`), so there's nothing to drive one from for anyone
/// else's craft. Every nozzle still renders, just dark and quiet, on a
/// remote ship — a reasonable, visible placeholder rather than either
/// badly guessing intensity from frame-to-frame velocity deltas or hiding
/// the geometry outright.
pub struct ThrustersPlugin;

impl Plugin for ThrustersPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_thruster_flame_effect);
    }
}

/// Which control axis (and which sign of it) one nozzle responds to.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum ThrusterAxis {
    ThrottleForward,
    ThrottleReverse,
    YawPositive,
    YawNegative,
    PitchPositive,
    PitchNegative,
    RollPositive,
    RollNegative,
}

impl ThrusterAxis {
    /// How lit/active (0..1) this nozzle should be given the ship's current
    /// stick position — each axis only ever responds to its own sign of
    /// its own input; every other combination reads as fully dark and
    /// silent, so at any moment only the nozzles actually doing something
    /// are lit and spewing exhaust.
    fn intensity(self, throttle: f32, yaw: f32, pitch: f32, roll: f32) -> f32 {
        match self {
            Self::ThrottleForward => throttle.max(0.0),
            Self::ThrottleReverse => (-throttle).max(0.0),
            Self::YawPositive => yaw.max(0.0),
            Self::YawNegative => (-yaw).max(0.0),
            Self::PitchPositive => pitch.max(0.0),
            Self::PitchNegative => (-pitch).max(0.0),
            Self::RollPositive => roll.max(0.0),
            Self::RollNegative => (-roll).max(0.0),
        }
    }
}

/// One nozzle's mount point (chassis-local) and which axis drives it.
pub struct ThrusterMount {
    pub offset: Vec3,
    pub axis: ThrusterAxis,
}

const NOZZLE_RADIUS: f32 = 0.14;
const NOZZLE_LENGTH: f32 = 0.32;
/// Unlit base color — a dark, faintly metallic housing, not pure black, so
/// an unlit nozzle still reads as "a real part," not a rendering gap.
const HOUSING_COLOR: Color = Color::srgb(0.12, 0.12, 0.14);
/// Fully-lit emissive color/intensity — a hot blue-white, well above 1.0 so
/// it actually blooms (every `CarCamera` already carries `Bloom::NATURAL` —
/// see `camera.rs`) rather than just reading as a bright but flat sprite.
const LIT_EMISSIVE: LinearRgba = LinearRgba::rgb(2.5, 4.0, 9.0);
/// Particles/sec at full intensity — scaled down for a barely-active
/// nozzle (see `sync_thruster_glow`) so the plume visibly thins out rather
/// than snapping instantly between "off" and "full blast."
const MAX_SPAWN_RATE: f32 = 90.0;

/// Marks a nozzle entity so `sync_thruster_glow` can find and drive it —
/// separate from `ThrusterAxis` only so a query can filter on "any nozzle
/// at all" without also needing to know every possible axis variant.
#[derive(Component)]
pub struct ThrusterNozzle;

/// Named alias for `sync_thruster_glow`'s own query shape — `pub` so
/// `aircraft::sync_plane_thrusters`/`dropship::sync_dropship_thrusters`
/// (the only two callers) can declare the identical query without either
/// hand-writing it twice or tripping clippy's `type_complexity` lint.
pub type NozzleQuery<'w, 's> = Query<
    'w,
    's,
    (&'static ThrusterAxis, &'static MeshMaterial3d<StandardMaterial>, Option<&'static mut EffectSpawner>),
    With<ThrusterNozzle>,
>;

/// The shared exhaust-plume `EffectAsset`, built once at startup — every
/// nozzle on every ship spawns its own independent `ParticleEffect`
/// instance of this same asset (see `spawn_thrusters`), the same "one
/// asset, many instances" shape `weapon_fx.rs`'s muzzle smoke already
/// uses. `pub` (field included) so `aircraft.rs`/`dropship.rs`'s own
/// `On<Insert, ...>` visual-setup observers can pass the handle straight
/// through to `spawn_thrusters`.
#[derive(Resource)]
pub struct ThrusterFlameEffect(pub Handle<EffectAsset>);

/// A short, hot plume: particles born right at the nozzle, drifting
/// outward along local `+Y` (see `spawn_thrusters`' own docs on why every
/// nozzle is rotated so `+Y` already points its own correct outward
/// direction) and fading from a bright blue-white core to nothing over a
/// fraction of a second — continuous while active (`SpawnerSettings::rate`,
/// toggled via `EffectSpawner::active` in `sync_thruster_glow`), not a
/// one-shot burst like a muzzle flash.
fn create_thruster_flame_effect() -> EffectAsset {
    let writer = ExprWriter::new();

    let init_pos = SetPositionSphereModifier {
        center: writer.lit(Vec3::ZERO).expr(),
        radius: writer.lit(0.03).expr(),
        dimension: ShapeDimension::Volume,
    };

    // Outward along local +Y (the nozzle's own barrel axis — see
    // `spawn_thrusters`), with a little random spread and speed variation
    // so the plume reads as turbulent exhaust, not a laser-straight jet.
    let spread = (writer.rand(VectorType::VEC3F) * writer.lit(2.0) - writer.lit(1.0)) * writer.lit(0.3);
    let speed = writer.lit(3.5) + writer.rand(ScalarType::Float) * writer.lit(2.5);
    let vel = (writer.lit(Vec3::Y) * speed + spread).expr();
    let init_vel = SetAttributeModifier::new(Attribute::VELOCITY, vel);

    let init_age = SetAttributeModifier::new(Attribute::AGE, writer.lit(0.0).expr());
    let init_lifetime =
        SetAttributeModifier::new(Attribute::LIFETIME, writer.lit(0.12).uniform(writer.lit(0.28)).expr());

    let update_drag = LinearDragModifier::new(writer.lit(0.6).expr());

    let mut color_gradient = bevy_hanabi::Gradient::new();
    // Values above 1.0 on purpose — same HDR-bloom reasoning
    // `LIT_EMISSIVE` gives the nozzle housing itself.
    color_gradient.add_key(0.0, Vec4::new(2.5, 4.0, 9.0, 1.0));
    color_gradient.add_key(1.0, Vec4::new(0.3, 0.5, 1.2, 0.0));

    let mut size_gradient = bevy_hanabi::Gradient::new();
    size_gradient.add_key(0.0, Vec3::splat(0.1));
    size_gradient.add_key(1.0, Vec3::splat(0.02));

    EffectAsset::new(128, SpawnerSettings::rate(MAX_SPAWN_RATE.into()), writer.finish())
        .with_name("thruster_flame")
        .init(init_pos)
        .init(init_vel)
        .init(init_age)
        .init(init_lifetime)
        .update(update_drag)
        .render(ColorOverLifetimeModifier::new(color_gradient))
        .render(SizeOverLifetimeModifier { gradient: size_gradient, screen_space_size: false })
}

fn setup_thruster_flame_effect(mut commands: Commands, mut effects: ResMut<Assets<EffectAsset>>) {
    commands.insert_resource(ThrusterFlameEffect(effects.add(create_thruster_flame_effect())));
}

/// Spawns one nozzle per `mounts` entry — a housing cylinder plus its own
/// exhaust-plume particle emitter, both on the same entity. Rotated so
/// local `+Y` (a `Cylinder`'s own default length axis, and the direction
/// `create_thruster_flame_effect`'s particles are born moving along) points
/// straight along `mount.offset` itself — i.e. *outward*, continuing
/// further in the same direction the mount already sits from the vehicle's
/// own center. That's a correct-enough exhaust direction for every mount
/// here without needing per-axis special-casing: a reverse thruster at the
/// nose sits at `-Z` and so sprays further toward `-Z`, a wingtip roll
/// thruster sits out at `+-X` and sprays further sideways, and so on.
/// Always spawned dark and inactive (`HOUSING_COLOR`, zero emissive,
/// `EffectSpawner` starts inactive via the asset's own `SpawnerSettings`);
/// `sync_thruster_glow` is what actually lights/activates any of them,
/// every frame, for whichever ship is being piloted.
pub fn spawn_thrusters(
    parent: &mut ChildSpawnerCommands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    flame_effect: &Handle<EffectAsset>,
    mounts: &[ThrusterMount],
) {
    // `TERRAIN_CAR_NO_PARTICLES=1` skips attaching the actual
    // `ParticleEffect`/hanabi simulation instance to every nozzle of every
    // plane/dropship in the world (not just the locally-piloted one — every
    // craft gets one per mount) while keeping the housing mesh itself, so
    // this can be A/B tested against a reported CPU cost without losing the
    // rest of the vehicle's visuals. See `weapon_fx.rs`'s muzzle smoke for
    // the same toggle applied to the other hanabi effect in this game.
    let no_particles = std::env::var("TERRAIN_CAR_NO_PARTICLES").is_ok();
    let mesh = meshes.add(Cylinder::new(NOZZLE_RADIUS, NOZZLE_LENGTH));
    for mount in mounts {
        let material = materials.add(StandardMaterial {
            base_color: HOUSING_COLOR,
            emissive: LinearRgba::BLACK,
            ..default()
        });
        let outward = mount.offset.normalize_or_zero();
        let rotation = if outward == Vec3::ZERO { Quat::IDENTITY } else { Quat::from_rotation_arc(Vec3::Y, outward) };
        let mut nozzle = parent.spawn((
            Mesh3d(mesh.clone()),
            MeshMaterial3d(material),
            Transform::from_translation(mount.offset).with_rotation(rotation),
            mount.axis,
            ThrusterNozzle,
        ));
        if !no_particles {
            nozzle.insert(ParticleEffect::new(flame_effect.clone()));
        }
    }
}

/// Lights and activates whichever of `entity`'s direct children are
/// thruster nozzles, each to `axis.intensity(...)` — called once per frame
/// per locally-piloted ship (see `aircraft::sync_plane_thrusters`/
/// `dropship::sync_dropship_thrusters`, the only two call sites). Recolors/
/// retoggles every nozzle unconditionally (not just ones that changed) —
/// cheap at the handful of nozzles one ship carries, and simpler than
/// tracking which ones were active last frame to know which to shut off
/// again. `EffectSpawner` is `Option` in the query: it's only inserted by
/// `bevy_hanabi`'s own systems a frame or two after `ParticleEffect` is
/// first added, so a brand-new nozzle briefly has none yet — skipped, not
/// an error, since the housing's own glow still updates regardless.
pub fn sync_thruster_glow(
    materials: &mut Assets<StandardMaterial>,
    children: &Children,
    nozzles: &mut NozzleQuery,
    throttle: f32,
    yaw: f32,
    pitch: f32,
    roll: f32,
) {
    for child in children.iter() {
        let Ok((axis, material, spawner)) = nozzles.get_mut(child) else {
            continue;
        };
        let intensity = axis.intensity(throttle, yaw, pitch, roll);
        if let Some(mut mat) = materials.get_mut(&material.0) {
            mat.emissive = LIT_EMISSIVE * intensity;
        }
        if let Some(mut spawner) = spawner {
            // A plain on/off toggle, not a live rate rewrite every frame —
            // `EffectSpawner::settings` reassignment mid-cycle is untested
            // territory for how gracefully `bevy_hanabi` handles it; a
            // simple threshold keeps this robust. The housing's own glow
            // (above) still scales continuously with `intensity`, so a
            // barely-active nozzle still reads as "just starting to fire."
            spawner.active = intensity > 0.05;
        }
    }
}
