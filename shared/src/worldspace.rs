use bevy::math::DVec3;
use bevy::prelude::*;

/// How far (in local/render units) an entity can wander from the current
/// local origin before everything gets re-centered. Comfortably inside
/// f32's precision budget — single precision starts losing sub-millimeter
/// accuracy well before this, and Rapier's own contact solving gets shaky
/// on similarly large coordinates. Shared so client and (once it exists)
/// server agree on when a rebase should happen.
pub const REBASE_THRESHOLD: f32 = 3000.0;

/// True position of local (0, 0, 0) in "universe" space, in meters, at f64
/// precision. Every Transform Bevy/Rapier actually touch is f32 and stays
/// close to zero; add this offset to a local position to recover its true
/// coordinate — that's what terrain generation samples from
/// (terrain_gen.rs), so content never shifts when a rebase happens.
///
/// f64 alone stays sub-meter-accurate out past a thousand light-years; the
/// piece that actually lets the world span a *whole galaxy* without
/// overflowing is the chunk grid using i64 coordinates (see terrain_gen.rs)
/// — this resource just keeps the local render/physics frame numerically
/// sane no matter how far `offset` has drifted.
#[derive(Resource, Default, Clone, Copy)]
pub struct WorldOrigin {
    pub offset: DVec3,
}

impl WorldOrigin {
    pub fn to_true(&self, local: Vec3) -> DVec3 {
        self.offset + local.as_dvec3()
    }
}
