//! Hashing of 64-bit keys and a small open-addressing map.

use std::hash::{BuildHasherDefault, Hasher};

#[inline]
pub fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    x
}

/// Maps a hash to `0..n` with its high 32 bits, leaving the low bits for tables.
#[inline]
pub fn bucket(hash: u64, n: usize) -> usize {
    (((hash >> 32) * n as u64) >> 32) as usize
}

/// Open addressing from non-zero u64 keys to u32 values (0 marks an empty slot).
pub struct U64Map {
    keys: Vec<u64>,
    values: Vec<u32>,
    mask: usize,
    len: usize,
}

impl U64Map {
    pub fn with_capacity(n: usize) -> Self {
        let slots = (n.max(8) * 2).next_power_of_two();
        Self {
            keys: vec![0; slots],
            values: vec![0; slots],
            mask: slots - 1,
            len: 0,
        }
    }

    /// The value for `key`, inserting `value` if it is absent; true if inserted.
    #[inline]
    pub fn get_or_insert(&mut self, key: u64, value: u32) -> (u32, bool) {
        if (self.len + 1) * 2 > self.keys.len() {
            self.grow();
        }
        let mut i = mix(key) as usize & self.mask;
        loop {
            let k = self.keys[i];
            if k == key {
                return (self.values[i], false);
            }
            if k == 0 {
                self.keys[i] = key;
                self.values[i] = value;
                self.len += 1;
                return (value, true);
            }
            i = (i + 1) & self.mask;
        }
    }

    fn grow(&mut self) {
        let mut bigger = U64Map::with_capacity(self.keys.len());
        for (i, &k) in self.keys.iter().enumerate() {
            if k != 0 {
                bigger.get_or_insert(k, self.values[i]);
            }
        }
        *self = bigger;
    }
}

/// A fast hasher for std maps keyed by u64.
#[derive(Default)]
pub struct MixHasher(u64);

impl Hasher for MixHasher {
    fn finish(&self) -> u64 {
        mix(self.0)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 ^= n;
    }
}

pub type MixBuild = BuildHasherDefault<MixHasher>;
pub type FastMap<K, V> = std::collections::HashMap<K, V, MixBuild>;
