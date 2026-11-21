//! Big scatter buffers without page-fault storms: one contiguous allocation per
//! thread, backed by transparent huge pages on Linux, cut into a fixed-capacity
//! region per partition. A partition that outgrows its region spills into a Vec.

use std::marker::PhantomData;

/// Raw memory for `n` values of T: anonymous huge-page mapping on Linux, a plain
/// allocation elsewhere. Never freed before exit on Linux (the process ends first).
struct Region<T> {
    ptr: *mut T,
    #[cfg(not(target_os = "linux"))]
    _keep: Vec<std::mem::MaybeUninit<T>>,
}

#[cfg(target_os = "linux")]
const HUGE: usize = 2 << 20;

impl<T> Region<T> {
    #[cfg(target_os = "linux")]
    fn new(n: usize) -> Self {
        let bytes = (n.max(1) * std::mem::size_of::<T>()).div_ceil(HUGE) * HUGE;
        // SAFETY: a fresh private anonymous mapping; we only hand out pointers into it.
        unsafe {
            let raw = libc::mmap(
                std::ptr::null_mut(),
                bytes + HUGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            );
            assert!(raw != libc::MAP_FAILED, "mmap of {bytes} bytes failed");
            let aligned = ((raw as usize + HUGE - 1) & !(HUGE - 1)) as *mut libc::c_void;
            let advice = if std::env::var("SS_THP").as_deref() == Ok("0") {
                libc::MADV_NOHUGEPAGE
            } else {
                libc::MADV_HUGEPAGE
            };
            libc::madvise(aligned, bytes, advice);
            Self { ptr: aligned as *mut T }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn new(n: usize) -> Self {
        let bytes = (n.max(1) * std::mem::size_of::<T>()).max(64);
        let layout = std::alloc::Layout::from_size_align(bytes, 4096).unwrap();
        // SAFETY: a non-zero-sized layout; the memory is never freed (the process ends first).
        let ptr = unsafe { std::alloc::alloc(layout) } as *mut T;
        assert!(!ptr.is_null(), "allocation of {bytes} bytes failed");
        Self { ptr, _keep: Vec::new() }
    }
}

/// A zero-filled slice of n values that lives until exit, on huge pages on Linux.
/// T must be valid when all-zero.
pub fn zeroed_slice<T: Copy>(n: usize) -> &'static mut [T] {
    let region = Region::<T>::new(n);
    let ptr = region.ptr;
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: the allocation holds n values; zero them, then keep it alive forever.
        unsafe { std::ptr::write_bytes(ptr, 0, n.max(1)) };
        std::mem::forget(region);
    }
    // SAFETY: n values of zeroed (anonymous mappings are zero-filled) memory, never freed.
    unsafe { std::slice::from_raw_parts_mut(ptr, n) }
}

/// A Vec of `n` values whose buffer is advised to use transparent huge pages,
/// so filling it costs a few faults instead of one per 4 KiB.
pub fn huge_vec<T>(n: usize) -> Vec<T> {
    let v: Vec<T> = Vec::with_capacity(n);
    #[cfg(target_os = "linux")]
    {
        const HUGE_PAGE: usize = 2 << 20;
        let start = (v.as_ptr() as usize).div_ceil(HUGE_PAGE) * HUGE_PAGE;
        let end = (v.as_ptr() as usize + n * std::mem::size_of::<T>()) / HUGE_PAGE * HUGE_PAGE;
        if end > start {
            // SAFETY: advice on memory this Vec owns.
            unsafe {
                libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_HUGEPAGE);
            }
        }
    }
    v
}

/// Values scattered into partitions, stored in blocks carved in order from one
/// per-thread arena: the memory touched (and zeroed by the kernel) is the data
/// actually written, not a reserved capacity per partition.
pub struct Scatter<T: Copy> {
    region: Region<T>,
    capacity: usize,
    used: usize,
    block: usize,
    /// Per partition: its blocks, and how full the last one is.
    blocks: Vec<Vec<*mut T>>,
    current: Vec<*mut T>,
    fill: Vec<usize>,
    /// Blocks handed over by `adopt` (memory already touched), used before the arena.
    free: Vec<*mut T>,
    overflow: Vec<Vec<T>>,
    /// Software write-combining: per partition, one cache line of values waiting
    /// to be written with non-temporal stores (when a line holds whole values).
    staging: Vec<Line>,
    staged: Vec<u8>,
    per_line: usize,
    _marker: PhantomData<T>,
}

#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Line([u8; 64]);

/// Block size of the write-combined scatters, the same for every record type so
/// that blocks can move from one scatter to another.
pub const BLOCK_BYTES: usize = 4096;

/// Copies one 64-byte line with non-temporal stores (no read of the destination).
#[inline(always)]
unsafe fn stream_line(dst: *mut u8, src: *const u8) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        let (d, s) = (dst as *mut __m128i, src as *const __m128i);
        _mm_stream_si128(d, _mm_load_si128(s));
        _mm_stream_si128(d.add(1), _mm_load_si128(s.add(1)));
        _mm_stream_si128(d.add(2), _mm_load_si128(s.add(2)));
        _mm_stream_si128(d.add(3), _mm_load_si128(s.add(3)));
    }
    #[cfg(not(target_arch = "x86_64"))]
    std::ptr::copy_nonoverlapping(src, dst, 64);
}

// SAFETY: a Scatter owns its memory exclusively, like a Vec<T>.
unsafe impl<T: Copy + Send> Send for Scatter<T> {}
unsafe impl<T: Copy + Sync> Sync for Scatter<T> {}

impl<T: Copy> Scatter<T> {
    /// `parts` partitions, about `expected` values in all (an estimate), room for
    /// `capacity` values in the arena before spilling into Vecs.
    pub fn new(parts: usize, expected: usize, capacity: usize) -> Self {
        let size = std::mem::size_of::<T>();
        let per_line = if size > 0 && size <= 64 && 64 % size == 0 { 64 / size } else { 0 };
        let block = if per_line > 0 {
            BLOCK_BYTES / size // whole lines, and the same bytes for every such T
        } else {
            (expected / parts.max(1) / 8).clamp(16, 1024)
        };
        let capacity = capacity + parts * block;
        Self {
            region: Region::new(capacity),
            capacity,
            used: 0,
            block,
            blocks: (0..parts).map(|_| Vec::new()).collect(),
            current: vec![std::ptr::null_mut(); parts],
            fill: vec![block; parts],
            free: Vec::new(),
            overflow: (0..parts).map(|_| Vec::new()).collect(),
            staging: vec![Line([0; 64]); if per_line > 0 { parts } else { 0 }],
            staged: vec![0; parts],
            per_line,
            _marker: PhantomData,
        }
    }

    /// Like push, through the partition's staging line (T must divide 64 bytes).
    /// `finish` must run before the values are read.
    #[inline(always)]
    pub fn push_wc(&mut self, p: usize, value: T) {
        debug_assert!(self.per_line > 0);
        let n = self.staged[p] as usize;
        // SAFETY: n < per_line, so the value lies inside the 64-byte line.
        unsafe { (self.staging.as_mut_ptr().add(p) as *mut T).add(n).write(value) };
        if n + 1 < self.per_line {
            self.staged[p] = (n + 1) as u8;
        } else {
            self.staged[p] = 0;
            self.flush_line(p);
        }
    }

    /// A block for partition p (recycled first), or None when the arena is full.
    #[inline]
    fn new_block(&mut self, p: usize) -> Option<*mut T> {
        let b = match self.free.pop() {
            Some(b) => b,
            None if self.used + self.block <= self.capacity => {
                // SAFETY: used + block <= capacity, inside the region.
                let b = unsafe { self.region.ptr.add(self.used) };
                self.used += self.block;
                b
            }
            None => return None,
        };
        self.blocks[p].push(b);
        self.current[p] = b;
        self.fill[p] = 0;
        Some(b)
    }

    #[inline(never)]
    fn flush_line(&mut self, p: usize) {
        if self.fill[p] == self.block && self.new_block(p).is_none() {
            // SAFETY: the staging line holds per_line written values.
            let values = unsafe { std::slice::from_raw_parts(self.staging.as_ptr().add(p) as *const T, self.per_line) };
            self.overflow[p].extend_from_slice(values);
            return;
        }
        // SAFETY: blocks are whole, 64-byte aligned lines and fill counts whole lines.
        unsafe {
            let dst = self.current[p].add(self.fill[p]) as *mut u8;
            stream_line(dst, self.staging.as_ptr().add(p) as *const u8);
        }
        self.fill[p] += self.per_line;
    }

    /// The blocks of partition p as raw memory, to be handed to `adopt` once its
    /// values are no longer needed.
    pub fn blocks_of(&self, p: usize) -> &[*mut T] {
        &self.blocks[p]
    }

    /// Takes over blocks of BLOCK_BYTES bytes (from another Scatter whose values in
    /// them are dead) to fill before carving fresh arena memory.
    pub fn adopt<U>(&mut self, blocks: &[*mut U]) {
        debug_assert!(self.block * std::mem::size_of::<T>() == BLOCK_BYTES);
        self.free.extend(blocks.iter().map(|&b| b as *mut T));
    }

    /// Writes out the partly filled staging lines; after this only `push` may be used.
    pub fn finish(&mut self) {
        for p in 0..self.staged.len() {
            let n = self.staged[p] as usize;
            for i in 0..n {
                // SAFETY: the first n values of the line were written by push_wc.
                let value = unsafe { (self.staging.as_ptr().add(p) as *const T).add(i).read() };
                self.push(p, value);
            }
            self.staged[p] = 0;
        }
        self.per_line = 0;
        #[cfg(target_arch = "x86_64")]
        // SAFETY: a store fence orders the non-temporal stores before later ones.
        unsafe {
            std::arch::x86_64::_mm_sfence()
        };
    }

    pub fn parts(&self) -> usize {
        self.blocks.len()
    }

    #[inline]
    pub fn push(&mut self, p: usize, value: T) {
        let f = self.fill[p];
        if f < self.block {
            // SAFETY: current[p] + f lies inside the partition's current block.
            unsafe { self.current[p].add(f).write(value) };
            self.fill[p] = f + 1;
        } else {
            self.push_new_block(p, value);
        }
    }

    #[cold]
    fn push_new_block(&mut self, p: usize, value: T) {
        match self.new_block(p) {
            Some(b) => {
                // SAFETY: b is the start of a block of `block` values.
                unsafe { b.write(value) };
                self.fill[p] = 1;
            }
            None => self.overflow[p].push(value),
        }
    }

    /// The values of partition p, as slices.
    #[inline]
    pub fn part(&self, p: usize) -> impl Iterator<Item = &[T]> + '_ {
        let blocks = &self.blocks[p];
        let last = blocks.len().wrapping_sub(1);
        blocks
            .iter()
            .enumerate()
            .map(move |(i, &b)| {
                let len = if i == last { self.fill[p] } else { self.block };
                // SAFETY: the first len values of the block were written by push.
                unsafe { std::slice::from_raw_parts(b as *const T, len) }
            })
            .chain(std::iter::once(self.overflow[p].as_slice()))
    }

    pub fn len(&self, p: usize) -> usize {
        let n = self.blocks[p].len();
        let in_blocks = if n == 0 { 0 } else { (n - 1) * self.block + self.fill[p] };
        in_blocks + self.overflow[p].len()
    }

    pub fn total(&self) -> usize {
        (0..self.parts()).map(|p| self.len(p)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::Scatter;

    #[test]
    fn write_combining_matches() {
        for capacity in [0, 7, 64, 100, 5000] {
            let mut s: Scatter<[u64; 4]> = Scatter::new(5, 300, capacity);
            for i in 0..3001u64 {
                s.push_wc((i * 7 % 5) as usize, [i, i + 1, i + 2, i + 3]);
            }
            s.finish();
            for p in 0..5 {
                let all: Vec<u64> = s.part(p).flatten().map(|v| v[0]).collect();
                assert_eq!(all, (0..3001).filter(|i| i * 7 % 5 == p as u64).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn adopted_blocks_are_reused() {
        let mut a: Scatter<[u64; 4]> = Scatter::new(3, 1000, 2000);
        for i in 0..1500u64 {
            a.push_wc((i % 3) as usize, [i; 4]);
        }
        a.finish();
        let mut b: Scatter<[u64; 2]> = Scatter::new(2, 100, 0); // almost no arena of its own
        for p in 0..3 {
            let blocks: Vec<_> = a.blocks_of(p).to_vec();
            b.adopt(&blocks);
        }
        for i in 0..1200u64 {
            b.push_wc((i % 2) as usize, [i, i]);
        }
        b.finish();
        for p in 0..2 {
            let all: Vec<u64> = b.part(p).flatten().map(|v| v[0]).collect();
            assert_eq!(all, (0..1200).filter(|i| i % 2 == p as u64).collect::<Vec<_>>());
        }
    }

    #[test]
    fn blocks_spill_and_keep_order() {
        for capacity in [0, 5, 40, 1000] {
            let mut s: Scatter<u64> = Scatter::new(3, 48, capacity);
            for i in 0..3000u64 {
                s.push((i % 3) as usize, i);
            }
            for p in 0..3 {
                let all: Vec<u64> = s.part(p).flatten().copied().collect();
                assert_eq!(all, (0..3000).filter(|i| i % 3 == p as u64).collect::<Vec<_>>());
                assert_eq!(s.len(p), 1000);
            }
            assert_eq!(s.total(), 3000);
        }
    }
}
