//! Linked IDs: union-find over every link, each person named by the byte-wise
//! smallest of their IDs.

use std::cmp::Ordering;

use crate::hash::{mix, U64Map};
use crate::key;

/// Open addressing from a linked user key to its person's name: one 16-byte slot
/// (key, name) per entry, so a lookup touches a single cache line. Key 0 is empty.
pub struct Names {
    slots: Vec<[u64; 2]>,
    mask: usize,
}

impl Names {
    fn with_entries(n: usize) -> Self {
        let size = (n.max(8) * 2).next_power_of_two();
        Self {
            slots: vec![[0, 0]; size],
            mask: size - 1,
        }
    }

    fn insert(&mut self, user: u64, name: u64) {
        let mut i = mix(user) as usize & self.mask;
        while self.slots[i][0] != 0 {
            i = (i + 1) & self.mask;
        }
        self.slots[i] = [user, name];
    }

    /// The person's name for a user key: itself if it appears in no link.
    #[inline]
    pub fn get(&self, user: u64) -> u64 {
        let mut i = mix(user) as usize & self.mask;
        loop {
            let [k, name] = self.slots[i];
            if k == user {
                return name;
            }
            if k == 0 {
                return user;
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Starts loading the slot `get(user)` will read first.
    #[inline]
    pub fn prefetch(&self, user: u64) {
        let i = mix(user) as usize & self.mask;
        prefetch(self.slots.as_ptr().wrapping_add(i));
    }
}

#[inline]
pub fn prefetch<T>(ptr: *const T) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: prefetching never faults, whatever the address.
    unsafe {
        std::arch::x86_64::_mm_prefetch(ptr as *const i8, std::arch::x86_64::_MM_HINT_T0);
    }
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let grand = parent[parent[x as usize] as usize];
        parent[x as usize] = grand;
        x = grand;
    }
    x
}

pub fn build(pairs: &[Vec<(u64, u64)>]) -> Names {
    let total: usize = pairs.iter().map(Vec::len).sum();
    let mut index = U64Map::with_capacity(total + 16);
    let mut keys: Vec<u64> = Vec::with_capacity(total + 16);
    let mut parent: Vec<u32> = Vec::with_capacity(total + 16);
    let mut size: Vec<u32> = Vec::with_capacity(total + 16);
    let mut node = |k: u64, keys: &mut Vec<u64>, parent: &mut Vec<u32>, size: &mut Vec<u32>| -> u32 {
        let (i, inserted) = index.get_or_insert(k, keys.len() as u32);
        if inserted {
            keys.push(k);
            parent.push(i);
            size.push(1);
        }
        i
    };
    for list in pairs {
        for &(a, b) in list {
            let ia = node(a, &mut keys, &mut parent, &mut size);
            let ib = node(b, &mut keys, &mut parent, &mut size);
            let ra = find(&mut parent, ia);
            let rb = find(&mut parent, ib);
            if ra != rb {
                let (big, small) = if size[ra as usize] >= size[rb as usize] { (ra, rb) } else { (rb, ra) };
                parent[small as usize] = big;
                size[big as usize] += size[small as usize];
            }
        }
    }
    let n = keys.len();
    let mut best = vec![0u64; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        if best[r] == 0 || key::cmp(keys[i], best[r]) == Ordering::Less {
            best[r] = keys[i];
        }
    }
    let mut names = Names::with_entries(n);
    for i in 0..n {
        let name = best[find(&mut parent, i as u32) as usize];
        names.insert(keys[i], name);
    }
    drop(index);
    names
}
