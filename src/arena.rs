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
        let mut keep = Vec::with_capacity(n.max(1));
        let ptr = keep.as_mut_ptr() as *mut T;
        Self { ptr, _keep: keep }
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

pub struct Scatter<T: Copy> {
    region: Region<T>,
    cap: usize,
    lens: Vec<u32>,
    overflow: Vec<Vec<T>>,
    _marker: PhantomData<T>,
}

// SAFETY: a Scatter owns its memory exclusively, like a Vec<T>.
unsafe impl<T: Copy + Send> Send for Scatter<T> {}
unsafe impl<T: Copy + Sync> Sync for Scatter<T> {}

impl<T: Copy> Scatter<T> {
    /// `parts` partitions of `cap` values each before spilling.
    pub fn new(parts: usize, cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            region: Region::new(parts * cap),
            cap,
            lens: vec![0; parts],
            overflow: (0..parts).map(|_| Vec::new()).collect(),
            _marker: PhantomData,
        }
    }

    pub fn parts(&self) -> usize {
        self.lens.len()
    }

    #[inline]
    pub fn push(&mut self, p: usize, value: T) {
        let len = self.lens[p] as usize;
        if len < self.cap {
            // SAFETY: p * cap + len is inside the region (len < cap, p < parts).
            unsafe { self.region.ptr.add(p * self.cap + len).write(value) };
            self.lens[p] += 1;
        } else {
            self.overflow[p].push(value);
        }
    }

    /// The values of partition p, as the region part and the spilled part.
    #[inline]
    pub fn part(&self, p: usize) -> [&[T]; 2] {
        // SAFETY: the first lens[p] values of the region part were written by push.
        let main = unsafe { std::slice::from_raw_parts(self.region.ptr.add(p * self.cap), self.lens[p] as usize) };
        [main, self.overflow[p].as_slice()]
    }

    pub fn len(&self, p: usize) -> usize {
        self.lens[p] as usize + self.overflow[p].len()
    }

    pub fn total(&self) -> usize {
        (0..self.parts()).map(|p| self.len(p)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::Scatter;

    #[test]
    fn spills_and_keeps_order() {
        let mut s: Scatter<u64> = Scatter::new(3, 4);
        for i in 0..30u64 {
            s.push((i % 3) as usize, i);
        }
        for p in 0..3 {
            let [a, b] = s.part(p);
            let all: Vec<u64> = a.iter().chain(b).copied().collect();
            assert_eq!(all, (0..30).filter(|i| i % 3 == p as u64).collect::<Vec<_>>());
        }
        assert_eq!(s.total(), 30);
    }
}
