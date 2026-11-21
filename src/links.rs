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

/// Union-find with path halving over dense nodes: `node[i]` is (parent, size).
#[inline]
fn find(node: &mut [[u32; 2]], mut x: u32) -> u32 {
    while node[x as usize][0] != x {
        let grand = node[node[x as usize][0] as usize][0];
        node[x as usize][0] = grand;
        x = grand;
    }
    x
}

/// Union-find over every linked ID. The big table maps a key to a dense node
/// number (its slot's second word during the build); the unions run on a small
/// dense array; a last sequential pass writes each slot's name.
///
/// Every random access is software-pipelined: the table slots of pair i + 2L are
/// prefetched, pair i + L gets its node numbers (and its nodes are prefetched),
/// and pair i is united.
pub fn build(pairs: &[Vec<(u64, u64)>]) -> Names {
    const L: usize = 16;
    let t0 = std::time::Instant::now();
    let total: usize = pairs.iter().map(Vec::len).sum();
    // At most 2 nodes per pair, about 1.4 in practice: 5 slots per pair keeps the
    // load near 0.3 and the table never full.
    let names = Names::with_slots(total * 5 + 64);
    let size = names.size;
    let mut keys: Vec<u64> = Vec::with_capacity(2 * total + 16);
    let mut node: Vec<[u32; 2]> = Vec::with_capacity(2 * total + 16);
    let t1 = t0.elapsed().as_secs_f64();

    let node_of = |slots: &mut [[u64; 2]], keys: &mut Vec<u64>, node: &mut Vec<[u32; 2]>, key: u64| -> u32 {
        let mut i = crate::hash::bucket(mix(key), size);
        loop {
            let k = slots[i][0];
            if k == key {
                return slots[i][1] as u32;
            }
            if k == 0 {
                let n = keys.len() as u32;
                slots[i] = [key, n as u64];
                keys.push(key);
                node.push([n, 1]);
                return n;
            }
            i += 1;
            if i == size {
                i = 0;
            }
        }
    };
    let mut far = pairs.iter().flatten();
    let mut near = pairs.iter().flatten();
    let mut ring = [(0u32, 0u32); L];
    for &(x, y) in far.by_ref().take(2 * L) {
        names.prefetch(x);
        names.prefetch(y);
    }
    let prefetch_node = |node: &Vec<[u32; 2]>, n: u32| prefetch(node.as_ptr().wrapping_add(n as usize));
    for (k, &(a, b)) in near.by_ref().take(L).enumerate() {
        let na = node_of(names.slots, &mut keys, &mut node, a);
        let nb = node_of(names.slots, &mut keys, &mut node, b);
        ring[k] = (na, nb);
    }
    for i in 0..total {
        let (na, nb) = ring[i % L];
        if let Some(&(x, y)) = far.next() {
            names.prefetch(x);
            names.prefetch(y);
        }
        let ra = find(&mut node, na);
        let rb = find(&mut node, nb);
        if ra != rb {
            let (big, small) = if node[ra as usize][1] >= node[rb as usize][1] { (ra, rb) } else { (rb, ra) };
            node[small as usize][0] = big;
            node[big as usize][1] += node[small as usize][1];
        }
        if let Some(&(a, b)) = near.next() {
            let na = node_of(names.slots, &mut keys, &mut node, a);
            let nb = node_of(names.slots, &mut keys, &mut node, b);
            prefetch_node(&node, na);
            prefetch_node(&node, nb);
            ring[i % L] = (na, nb);
        }
    }
    let t2 = t0.elapsed().as_secs_f64();
    // Each node's root (prefetching the parent of node i + L), then the smallest
    // key of each root, then each node's name, then every slot's.
    let n = keys.len();
    for i in 0..n {
        if let Some(ahead) = node.get(i + L) {
            prefetch_node(&node, ahead[0]);
        }
        let r = find(&mut node, i as u32);
        node[i][0] = r;
    }
    let mut best = vec![0u64; n];
    for i in 0..n {
        if let Some(ahead) = node.get(i + L) {
            prefetch(best.as_ptr().wrapping_add(ahead[0] as usize));
        }
        let r = node[i][0] as usize;
        if best[r] == 0 || key::cmp(keys[i], best[r]) == Ordering::Less {
            best[r] = keys[i];
        }
    }
    let mut name_of = keys; // reused: each node's key becomes its name
    for i in 0..n {
        if let Some(ahead) = node.get(i + L) {
            prefetch(best.as_ptr().wrapping_add(ahead[0] as usize));
        }
        name_of[i] = best[node[i][0] as usize];
    }
    let slots = &mut *names.slots;
    for j in 0..slots.len() {
        if let Some(ahead) = slots.get(j + L) {
            if ahead[0] != 0 {
                prefetch(name_of.as_ptr().wrapping_add(ahead[1] as usize));
            }
        }
        let slot = &mut slots[j];
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
