use bevy::math::DVec3;

use crate::chunk_rng::ChunkRng;
use crate::terrain_gen::{world_to_chunk, ChunkCoord, CHUNK_SIZE};

/// Chance any given chunk contains a deposit — sparse, since a deposit is
/// meant to be a notable "resource-rich spot" worth building on, unlike
/// `obstacles::obstacles_for_chunk`'s scattered rocks/trees.
const DEPOSIT_CHANCE: f64 = 0.08;
/// How close an `ExtractionFacility` must be placed to a deposit's point
/// to count as "on" it — see `is_near_deposit`.
pub const DEPOSIT_CLAIM_RADIUS: f64 = 15.0;
/// Salt distinguishing this content stream from `obstacles::obstacles_for_chunk`'s
/// own `ChunkRng` — same coordinate, unrelated sequence.
const DEPOSIT_SALT: u64 = 0xD05_1717E;

#[derive(Clone, Copy, Debug)]
pub struct DepositSpec {
    /// True-space (world-origin-relative) position — same convention as
    /// `terrain_gen::height_at`'s `x`/`z`, so this never shifts when
    /// `WorldOrigin` rebases.
    pub true_x: f64,
    pub true_z: f64,
}

/// Deterministic deposit placement for one terrain chunk — a pure function
/// of the chunk coordinate alone (unlike obstacles, no slope check: a
/// deposit is an abstract resource marker, not a physical prop needing
/// believable footing), so client and server independently agree without
/// ever exchanging deposit locations over the network — the same
/// determinism trick `terrain_gen`/`obstacles` already use.
pub fn deposit_for_chunk(coord: ChunkCoord) -> Option<DepositSpec> {
    let mut rng = ChunkRng::new(coord, DEPOSIT_SALT);
    if rng.next_f64() >= DEPOSIT_CHANCE {
        return None;
    }
    let size = CHUNK_SIZE as f64;
    let true_center_x = coord.0 as f64 * size;
    let true_center_z = coord.1 as f64 * size;
    let jitter_x = (rng.next_f64() - 0.5) * size;
    let jitter_z = (rng.next_f64() - 0.5) * size;
    Some(DepositSpec {
        true_x: true_center_x + jitter_x,
        true_z: true_center_z + jitter_z,
    })
}

/// Whether true-space `(true_x, true_z)` is close enough to *some* deposit
/// to place an `ExtractionFacility` there — checks the containing chunk
/// and its 8 neighbors (a deposit generated near a chunk edge could be
/// geometrically closer to a point technically inside the adjacent chunk).
/// Cheap enough to call once per placement attempt; never meant to run
/// per-frame.
pub fn is_near_deposit(true_x: f64, true_z: f64) -> bool {
    let (cx, cz) = world_to_chunk(DVec3::new(true_x, 0.0, true_z));
    for dx in -1..=1 {
        for dz in -1..=1 {
            let Some(deposit) = deposit_for_chunk((cx + dx, cz + dz)) else {
                continue;
            };
            let ddx = deposit.true_x - true_x;
            let ddz = deposit.true_z - true_z;
            if (ddx * ddx + ddz * ddz).sqrt() <= DEPOSIT_CLAIM_RADIUS {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_chunk_is_deterministic() {
        assert_eq!(
            deposit_for_chunk((5, -3)).map(|d| (d.true_x, d.true_z)),
            deposit_for_chunk((5, -3)).map(|d| (d.true_x, d.true_z)),
        );
    }

    #[test]
    fn a_deposits_own_point_is_near_itself() {
        // Search a range of chunks for one that actually has a deposit —
        // DEPOSIT_CHANCE means most don't, so scanning a handful is the
        // simplest way to get a real one for this test.
        let deposit = (0..200)
            .find_map(|i| deposit_for_chunk((i, 0)))
            .expect("expected at least one deposit within 200 chunks");
        assert!(is_near_deposit(deposit.true_x, deposit.true_z));
    }

    #[test]
    fn far_from_any_deposit_is_not_near() {
        // A point deep inside a chunk that itself has no deposit and is
        // far enough from its neighbors' possible deposit points too.
        let mut x = 0i64;
        while deposit_for_chunk((x, 1_000_000)).is_some() {
            x += 1;
        }
        let true_x = x as f64 * CHUNK_SIZE as f64;
        let true_z = 1_000_000.0 * CHUNK_SIZE as f64;
        assert!(!is_near_deposit(true_x, true_z));
    }
}
