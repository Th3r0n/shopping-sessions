//! Linked IDs: union-find over every link, each person named by the byte-wise
//! smallest of their IDs.

use std::cmp::Ordering;

use crate::hash::mix;
use crate::key;

/// Open addressing from a linked user key to its person's name: one 16-byte slot
/// (key, name) per entry, so a lookup touches a single cache line. Key 0 is empty.
pub struct Names {
    slots: &'static mut [[u64; 2]],
    size: usize,
}

impl Names {
    /// A table of exactly `size` slots (any size: home slots by multiply-shift).
    fn with_slots(size: usize) -> Self {
        let size = size.max(16);
        assert!(size < u32::MAX as usize, "name table too large");
        Self {
            slots: crate::arena::zeroed_slice(size),
            size,
        }
    }

    #[inline]
    fn home(&self, key: u64) -> usize {
        crate::hash::bucket(mix(key), self.size)
    }

    /// The person's name for a user key: itself if it appears in no link.
    #[inline]
    pub fn get(&self, user: u64) -> u64 {
        let mut i = self.home(user);
        loop {
            let [k, name] = self.slots[i];
            if k == user {
                return name;
            }
            if k == 0 {
                return user;
            }
            i += 1;
            if i == self.size {
                i = 0;
            }
        }
    }

    /// Starts loading the slot `get(user)` will read first.
    #[inline]
    pub fn prefetch(&self, user: u64) {
        let i = self.home(user);
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

/// Union-find with path halving over dense node numbers.
#[inline]
fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let grand = parent[parent[x as usize] as usize];
        parent[x as usize] = grand;
        x = grand;
    }
    x
}

/// Union-find over every linked ID. The big table maps a key to a dense node
/// number (its slot's second word during the build); the unions run on small
/// dense arrays; a last sequential pass writes each slot's name.
pub fn build(pairs: &[Vec<(u64, u64)>]) -> Names {
    const LAG: usize = 16;
    let t0 = std::time::Instant::now();
    let total: usize = pairs.iter().map(Vec::len).sum();
    // At most 2 nodes per pair, about 1.4 in practice: 5 slots per pair keeps the
    // load near 0.3 and the table never full.
    let names = Names::with_slots(total * 5 + 64);
    let size = names.size;
    let mut keys: Vec<u64> = Vec::with_capacity(2 * total + 16);
    let mut parent: Vec<u32> = Vec::with_capacity(2 * total + 16);
    let mut weight: Vec<u32> = Vec::with_capacity(2 * total + 16);
    let t1 = t0.elapsed().as_secs_f64();

    let node_of = |slots: &mut [[u64; 2]], keys: &mut Vec<u64>, parent: &mut Vec<u32>, weight: &mut Vec<u32>, key: u64| -> u32 {
        let mut i = crate::hash::bucket(mix(key), size);
        loop {
            let k = slots[i][0];
            if k == key {
                return slots[i][1] as u32;
            }
            if k == 0 {
                let node = keys.len() as u32;
                slots[i] = [key, node as u64];
                keys.push(key);
                parent.push(node);
                weight.push(1);
                return node;
            }
            i += 1;
            if i == size {
                i = 0;
            }
        }
    };
    let mut ahead = pairs.iter().flatten().skip(LAG);
    for &(a, b) in pairs.iter().flatten() {
        if let Some(&(x, y)) = ahead.next() {
            names.prefetch(x);
            names.prefetch(y);
        }
        let na = node_of(names.slots, &mut keys, &mut parent, &mut weight, a);
        let nb = node_of(names.slots, &mut keys, &mut parent, &mut weight, b);
        let ra = find(&mut parent, na);
        let rb = find(&mut parent, nb);
        if ra != rb {
            let (big, small) = if weight[ra as usize] >= weight[rb as usize] { (ra, rb) } else { (rb, ra) };
            parent[small as usize] = big;
            weight[big as usize] += weight[small as usize];
        }
    }
    let t2 = t0.elapsed().as_secs_f64();
    // The smallest key of each root, then each node's name, then every slot's.
    let n = keys.len();
    let mut best = vec![0u64; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        if best[r] == 0 || key::cmp(keys[i], best[r]) == Ordering::Less {
            best[r] = keys[i];
        }
    }
    let mut name_of = vec![0u64; n];
    for (i, name) in name_of.iter_mut().enumerate() {
        *name = best[find(&mut parent, i as u32) as usize];
    }
    for slot in names.slots.iter_mut() {
        if slot[0] != 0 {
            slot[1] = name_of[slot[1] as usize];
        }
    }
    if std::env::var_os("SS_DEBUG").is_some() {
        eprintln!(
            "links: alloc {t1:.3}s, unions {:.3}s, names {:.3}s, {} nodes, {} slots",
            t2 - t1,
            t0.elapsed().as_secs_f64() - t2,
            n,
            names.slots.len()
        );
    }
    names
}
