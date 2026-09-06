/// A small, fully deterministic PRNG seeded from a chunk coordinate plus a
/// salt (so different content types keyed off the same chunk — obstacles,
/// deposits, ... — don't accidentally produce correlated sequences).
/// Avoids pulling `rand` into `shared` for just this, and (unlike `rand`'s
/// own default algorithms, which don't promise cross-version stability)
/// guarantees the exact same sequence forever — the whole point is that
/// client, server, and every other connected client independently arrive
/// at the identical answer from nothing but a chunk coordinate, so this
/// content never needs to be replicated over the network. Extracted here
/// once a second consumer (`deposits.rs`) needed the exact same scheme
/// `obstacles.rs` already had, rather than duplicating it a second time.
pub struct ChunkRng(u64);

impl ChunkRng {
    pub fn new(coord: (i64, i64), salt: u64) -> Self {
        let x = coord.0 as u64;
        let z = coord.1 as u64;
        let mut h = x
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ z.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ salt;
        // SplitMix64 finalizer, to avoid an all-zero or low-entropy seed
        // from the xor above feeding straight into xorshift below.
        h ^= h >> 33;
        h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        h ^= h >> 33;
        Self(h | 1)
    }

    /// xorshift64, [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_coord_and_salt_is_deterministic() {
        let mut a = ChunkRng::new((3, -7), 0xF00D);
        let mut b = ChunkRng::new((3, -7), 0xF00D);
        for _ in 0..8 {
            assert_eq!(a.next_f64(), b.next_f64());
        }
    }

    #[test]
    fn different_salt_diverges() {
        let mut a = ChunkRng::new((3, -7), 0xF00D);
        let mut b = ChunkRng::new((3, -7), 0xBEEF);
        assert_ne!(a.next_f64(), b.next_f64());
    }
}
