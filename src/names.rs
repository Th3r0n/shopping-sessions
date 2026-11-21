//! Linked IDs on every thread: each person named by the byte-wise smallest of their
//! IDs (values of the user scheme, see ids.rs).
//!
//! One open-addressing table of 16-byte slots `[key + 1, word]` (0 = empty) holds
//! every linked ID; the slot index is the ID's union-find node. During the build
//! `word` is the parent's slot + 1 (0 = a root). Unions are lock-free: a root is
//! hung, by compare-and-swap, under the root whose ID sorts first, so the root of a
//! component is always its smallest ID, whatever the interleaving. Finds halve
//! paths with plain stores (a parent pointer only ever moves to an ancestor). Once
//! every union is done, `word` becomes the name's value + 1.
//!
//! A bit filter over a second hash marks the renamed IDs (name != itself): a user
//! whose bit is clear names itself without touching the table (most users).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

use crate::ids::{hash, hash2, Scheme};

pub struct Names {
    slots: &'static [[AtomicU64; 2]],
    shift: u32,
    filter: &'static [AtomicU64],
    fshift: u32,
}

/// The table would not hold every linked ID (never with the sizing below).
#[derive(Debug)]
pub struct Overflow;

fn atomic_slice<const N: usize>(n: usize) -> &'static [[AtomicU64; N]] {
    let raw: &'static mut [[u64; N]] = crate::arena::zeroed_slice(n);
    // SAFETY: AtomicU64 has the size and alignment of u64; the memory is zeroed and
    // lives until exit.
    unsafe { std::slice::from_raw_parts(raw.as_ptr() as *const [AtomicU64; N], n) }
}

#[inline]
fn prefetch<T>(ptr: *const T) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: prefetching never faults.
    unsafe {
        std::arch::x86_64::_mm_prefetch(ptr as *const i8, std::arch::x86_64::_MM_HINT_T0)
    };
}

impl Names {
    /// No linked IDs: everyone names themself.
    pub fn empty() -> Names {
        Names { slots: atomic_slice::<2>(16), shift: 64 - 4, filter: &[], fshift: 0 }
    }

    #[inline(always)]
    fn home(&self, v: u64) -> usize {
        (hash(v) >> self.shift) as usize
    }

    #[inline(always)]
    fn renamed_bit(&self, v: u64) -> (usize, u64) {
        let b = (hash2(v) >> self.fshift) as usize;
        (b / 64, 1u64 << (b % 64))
    }

    /// The person (name value) of a user value.
    #[inline(always)]
    pub fn get(&self, v: u64) -> u64 {
        if self.filter.is_empty() {
            return v;
        }
        let (w, bit) = self.renamed_bit(v);
        // SAFETY: w < filter.len() by construction of fshift.
        if unsafe { self.filter.get_unchecked(w) }.load(Relaxed) & bit == 0 {
            return v;
        }
        let mask = self.slots.len() - 1;
        let mut i = self.home(v);
        loop {
            // SAFETY: i <= mask.
            let slot = unsafe { self.slots.get_unchecked(i) };
            let k = slot[0].load(Relaxed);
            if k == v + 1 {
                return slot[1].load(Relaxed) - 1;
            }
            if k == 0 {
                return v;
            }
            i = (i + 1) & mask;
        }
    }

    /// Prefetches what `get` will read: the filter word, then the home slot.
    #[inline(always)]
    pub fn prefetch(&self, v: u64) {
        if !self.filter.is_empty() {
            prefetch(self.filter.as_ptr().wrapping_add(self.renamed_bit(v).0));
            prefetch(self.slots.as_ptr().wrapping_add(self.home(v)));
        }
    }

    /// The slot of `v`, inserted if new; None if the table is full.
    #[inline]
    fn insert(&self, v: u64, inserted: &mut usize) -> Option<usize> {
        let k = v + 1;
        let mask = self.slots.len() - 1;
        let mut i = self.home(v);
        for _ in 0..self.slots.len() {
            let slot = &self.slots[i][0];
            let cur = slot.load(Relaxed);
            if cur == k {
                return Some(i);
            }
            if cur == 0 {
                match slot.compare_exchange(0, k, Relaxed, Relaxed) {
                    Ok(_) => {
                        *inserted += 1;
                        return Some(i);
                    }
                    Err(now) if now == k => return Some(i),
                    Err(_) => {}
                }
            }
            i = (i + 1) & mask;
        }
        None
    }

    #[inline(always)]
    fn parent(&self, x: usize) -> usize {
        match self.slots[x][1].load(Relaxed) {
            0 => x,
            w => (w - 1) as usize,
        }
    }

    #[inline]
    fn find(&self, mut x: usize) -> usize {
        loop {
            let p = self.parent(x);
            if p == x {
                return x;
            }
            let g = self.parent(p);
            if g != p {
                self.slots[x][1].store(g as u64 + 1, Relaxed); // g is an ancestor of x
            }
            x = g;
        }
    }

    #[inline]
    fn value(&self, x: usize) -> u64 {
        self.slots[x][0].load(Relaxed) - 1
    }

    fn union(&self, a: usize, b: usize, users: &Scheme) {
        loop {
            let (ra, rb) = (self.find(a), self.find(b));
            if ra == rb {
                return;
            }
            // the root whose ID sorts last goes under the other one
            let (child, top) = if users.cmp(self.value(ra), self.value(rb)).is_gt() { (ra, rb) } else { (rb, ra) };
            if self.slots[child][1].compare_exchange(0, top as u64 + 1, Relaxed, Relaxed).is_ok() {
                return;
            }
        }
    }

    /// Builds the names of every linked ID on `threads` threads. `pairs` holds the
    /// (a, b) values of the kept link lines.
    pub fn build(pairs: &[Vec<(u64, u64)>], threads: usize, users: &Scheme) -> Result<Names, Overflow> {
        const TASK: usize = 1 << 14;
        const LAG: usize = 16;
        let total: usize = pairs.iter().map(Vec::len).sum();
        if total == 0 {
            return Ok(Names::empty());
        }
        // at most 2 nodes per pair: the load stays at or below 2/3
        let size = (3 * total).next_power_of_two().max(64);
        let mut names = Names { slots: atomic_slice::<2>(size), shift: 64 - size.trailing_zeros(), filter: &[], fshift: 0 };
        let tasks: Vec<(usize, usize, usize)> = pairs
            .iter()
            .enumerate()
            .flat_map(|(v, p)| (0..p.len()).step_by(TASK).map(move |s| (v, s, (s + TASK).min(p.len()))))
            .collect();
        let threads = threads.clamp(1, tasks.len().max(1));

        // 1. every endpoint into the table (prefetched LAG pairs ahead)
        let next = AtomicUsize::new(0);
        let failed = std::sync::atomic::AtomicBool::new(false);
        let mut nodes = 0usize;
        let mut slot_pairs: Vec<Vec<(u32, u32)>> = Vec::new();
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let (names, tasks, next, failed) = (&names, &tasks, &next, &failed);
                    s.spawn(move || {
                        let mut out = Vec::new();
                        let mut inserted = 0usize;
                        loop {
                            let t = next.fetch_add(1, Relaxed);
                            let Some(&(v, start, end)) = tasks.get(t) else { break };
                            let p = &pairs[v][start..end];
                            let mut sp = Vec::with_capacity(p.len());
                            for (j, &(a, b)) in p.iter().enumerate() {
                                if let Some(&(x, y)) = p.get(j + LAG) {
                                    prefetch(names.slots.as_ptr().wrapping_add(names.home(x)));
                                    prefetch(names.slots.as_ptr().wrapping_add(names.home(y)));
                                }
                                match (names.insert(a, &mut inserted), names.insert(b, &mut inserted)) {
                                    (Some(x), Some(y)) => sp.push((x as u32, y as u32)),
                                    _ => {
                                        failed.store(true, Relaxed);
                                        return (out, inserted);
                                    }
                                }
                            }
                            out.push(sp);
                        }
                        (out, inserted)
                    })
                })
                .collect();
            for h in handles {
                let (out, inserted) = h.join().unwrap();
                slot_pairs.extend(out);
                nodes += inserted;
            }
        });
        if failed.load(Relaxed) {
            return Err(Overflow);
        }

        // 2. unions (each pair's slots prefetched LAG pairs ahead)
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                let (names, slot_pairs, next) = (&names, &slot_pairs, &next);
                s.spawn(move || loop {
                    let t = next.fetch_add(1, Relaxed);
                    let Some(sp) = slot_pairs.get(t) else { break };
                    for (j, &(a, b)) in sp.iter().enumerate() {
                        if let Some(&(x, y)) = sp.get(j + LAG) {
                            prefetch(names.slots.as_ptr().wrapping_add(x as usize));
                            prefetch(names.slots.as_ptr().wrapping_add(y as usize));
                        }
                        names.union(a as usize, b as usize, users);
                    }
                });
            }
        });

        // 3. every node straight to its root; 4. every node's word becomes its name
        //    and the renamed ones get their filter bit
        let filter_bits = (nodes * 8).next_power_of_two().max(64);
        let filter = atomic_slice::<1>(filter_bits / 64);
        names.fshift = 64 - filter_bits.trailing_zeros();
        let ranges: Vec<(usize, usize)> = (0..size).step_by(1 << 16).map(|s| (s, (s + (1 << 16)).min(size))).collect();
        for step in 0..2 {
            let next = AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..threads {
                    let (names, ranges, next, filter) = (&names, &ranges, &next, filter);
                    s.spawn(move || loop {
                        let r = next.fetch_add(1, Relaxed);
                        let Some(&(start, end)) = ranges.get(r) else { break };
                        for i in start..end {
                            if names.slots[i][0].load(Relaxed) == 0 {
                                continue;
                            }
                            if step == 0 {
                                let root = names.find(i);
                                if root != i {
                                    names.slots[i][1].store(root as u64 + 1, Relaxed);
                                }
                            } else {
                                let own = names.value(i);
                                let name = match names.slots[i][1].load(Relaxed) {
                                    0 => own,
                                    w => names.value((w - 1) as usize),
                                };
                                names.slots[i][1].store(name + 1, Relaxed);
                                if name != own {
                                    let (w, bit) = names.renamed_bit(own);
                                    filter[w][0].fetch_or(bit, Relaxed);
                                }
                            }
                        }
                    });
                }
            });
        }
        // SAFETY: same layout ([AtomicU64; 1] is one AtomicU64).
        names.filter = unsafe { std::slice::from_raw_parts(filter.as_ptr() as *const AtomicU64, filter.len()) };
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Against a plain sequential union-find with names by byte order.
    #[test]
    fn names_match_a_sequential_build() {
        let users = Scheme::detect([&b"u1"[..], b"u2", b"u3"]);
        for round in 0..30u64 {
            let mut s = 0x9e37_79b9_7f4a_7c15 ^ round;
            let ids: Vec<Vec<u8>> = (0..(50 + round * 37))
                .map(|i| match rng(&mut s) % 9 {
                    0 => format!("U{i}").into_bytes(),
                    1 => format!("u0{i}").into_bytes(),
                    2 => format!("u{i} ").into_bytes(),
                    _ => format!("u{}", rng(&mut s) % 1_000_000).into_bytes(),
                })
                .collect();
            let mut ids = ids;
            ids.sort();
            ids.dedup();
            let values: Vec<u64> = ids.iter().map(|id| users.value(id)).collect();
            let mut pairs: Vec<Vec<(u64, u64)>> = vec![Vec::new(); 3];
            let mut edges = Vec::new();
            for k in 0..(ids.len() * (1 + round as usize % 3)) {
                let (a, b) = ((rng(&mut s) % ids.len() as u64) as usize, (rng(&mut s) % ids.len() as u64) as usize);
                let b = if k % 5 == 0 { a } else { b };
                pairs[k % 3].push((values[a], values[b]));
                edges.push((a, b));
            }
            // chains, as in the generator
            for k in 1..ids.len().min(40) {
                pairs[0].push((values[k - 1], values[k]));
                edges.push((k - 1, k));
            }
            let names = Names::build(&pairs, 1 + round as usize % 8, &users).unwrap();
            let mut parent: Vec<usize> = (0..ids.len()).collect();
            fn find(p: &mut Vec<usize>, mut x: usize) -> usize {
                while p[x] != x {
                    x = p[x];
                }
                x
            }
            for &(a, b) in &edges {
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                parent[ra] = rb;
            }
            for i in 0..ids.len() {
                let r = find(&mut parent, i);
                let best = (0..ids.len()).filter(|&j| find(&mut parent, j) == r).map(|j| &ids[j]).min().unwrap();
                let linked = edges.iter().any(|&(a, b)| a == i || b == i);
                let want = if linked { best.clone() } else { ids[i].clone() };
                assert_eq!(users.to_bytes(names.get(values[i])), want, "round {round} id {:?}", String::from_utf8_lossy(&ids[i]));
            }
            // unlinked IDs name themselves
            for x in [b"u999999999".as_slice(), b"Q", b"u77777777"] {
                let v = users.value(x);
                if !values.contains(&v) {
                    assert_eq!(names.get(v), v);
                }
            }
        }
    }
}
