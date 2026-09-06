use crate::chunk_rng::ChunkRng;
use crate::terrain_gen::{slope_at, TerrainNoise, CHUNK_SIZE};

/// Candidate placements tried per chunk — not all survive the slope check,
/// so actual obstacle density per chunk is somewhat lower and varies with
/// terrain steepness.
const ATTEMPTS_PER_CHUNK: usize = 6;
/// `1 - normal.y`; above this the ground is considered too steep for
/// anything to have plausibly ended up sitting there.
const MAX_SLOPE: f32 = 0.6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObstacleKind {
    Rock,
    Tree,
}

#[derive(Clone, Copy, Debug)]
pub struct ObstacleSpec {
    pub kind: ObstacleKind,
    /// True-space (world-origin-relative) position — same convention as
    /// `terrain_gen::height_at`'s `x`/`z`, for the same reason (content
    /// must not change when `WorldOrigin` rebases).
    pub true_x: f64,
    pub true_z: f64,
    pub scale: f32,
    pub rotation_y: f32,
}

/// Deterministic obstacle placement for one terrain chunk — a pure function
/// of the chunk's coordinate and the shared noise (used only for the slope
/// check, to keep obstacles off cliff faces), so client, server, and every
/// other connected client independently compute the identical set without
/// ever sending obstacle data over the network — the same trick
/// `terrain_gen::height_at` already uses for terrain itself (see that
/// module's docs).
pub fn obstacles_for_chunk(noise: &TerrainNoise, coord: (i64, i64)) -> Vec<ObstacleSpec> {
    let mut rng = ChunkRng::new(coord, 0xF00D);
    let size = CHUNK_SIZE as f64;
    let true_center_x = coord.0 as f64 * size;
    let true_center_z = coord.1 as f64 * size;

    let mut specs = Vec::new();
    for _ in 0..ATTEMPTS_PER_CHUNK {
        let jitter_x = (rng.next_f64() - 0.5) * size;
        let jitter_z = (rng.next_f64() - 0.5) * size;
        let x = true_center_x + jitter_x;
        let z = true_center_z + jitter_z;

        let normal = slope_at(noise, x, z, 4.0);
        let slope = 1.0 - normal.y;
        if slope > MAX_SLOPE {
            continue;
        }

        let kind = if rng.next_f64() < 0.5 {
            ObstacleKind::Rock
        } else {
            ObstacleKind::Tree
        };
        let scale = 0.7 + rng.next_f64() as f32;
        let rotation_y = rng.next_f64() as f32 * std::f32::consts::TAU;

        specs.push(ObstacleSpec {
            kind,
            true_x: x,
            true_z: z,
            scale,
            rotation_y,
        });
    }
    specs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The determinism this whole scheme depends on: client and server
    /// must never need to exchange obstacle data because both sides
    /// independently arrive at exactly the same answer. If this ever
    /// broke (e.g. from swapping in a non-deterministic RNG), the two
    /// sides would silently see obstacles in different places without any
    /// error — worth pinning down as a test, not just an assumption.
    #[test]
    fn same_chunk_and_seed_is_deterministic() {
        let noise = TerrainNoise::from_seed(42);
        let a = obstacles_for_chunk(&noise, (3, -7));
        let b = obstacles_for_chunk(&noise, (3, -7));
        assert_eq!(a.len(), b.len());
        for (spec_a, spec_b) in a.iter().zip(b.iter()) {
            assert_eq!(spec_a.kind, spec_b.kind);
            assert_eq!(spec_a.true_x, spec_b.true_x);
            assert_eq!(spec_a.true_z, spec_b.true_z);
            assert_eq!(spec_a.scale, spec_b.scale);
            assert_eq!(spec_a.rotation_y, spec_b.rotation_y);
        }
    }

    #[test]
    fn different_chunks_differ() {
        let noise = TerrainNoise::from_seed(42);
        let a = obstacles_for_chunk(&noise, (0, 0));
        let b = obstacles_for_chunk(&noise, (1, 0));
        // Not a proof of "no coincidental overlap," just a sanity check
        // that neighboring chunks aren't accidentally reusing the exact
        // same candidate offsets (which would suggest the coordinate isn't
        // actually feeding the RNG).
        assert_ne!(
            a.iter().map(|s| (s.true_x, s.true_z)).collect::<Vec<_>>(),
            b.iter().map(|s| (s.true_x, s.true_z)).collect::<Vec<_>>()
        );
    }
}
