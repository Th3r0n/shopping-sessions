//! Reading the CSV folders: files split into line-aligned chunks (planned up
//! front), each chunk read with pread into a reusable per-thread buffer padded
//! with `\n`, then split into the well-formed lines' fields (AVX2 when available).

use std::fs::File;
use std::path::Path;

use memchr::memchr;

use crate::csv;

/// Bytes of `\n` after a chunk in its buffer, so SIMD loads never need a bounds check.
pub const PAD: usize = 128;

static POPULATE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var("SS_POPULATE").as_deref() != Ok("0"));

/// Maps a chunk's pages in one call (Linux 5.14+) instead of one fault per page group.
fn populate(data: &[u8]) {
    #[cfg(target_os = "linux")]
    {
        const MADV_POPULATE_READ: libc::c_int = 22;
        let start = data.as_ptr() as usize & !4095;
        let end = data.as_ptr() as usize + data.len();
        // SAFETY: advice only; a failure (old kernel) is ignored.
        unsafe {
            libc::madvise(start as *mut libc::c_void, end - start, MADV_POPULATE_READ);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = data;
}

pub struct Input {
    pub files: Vec<File>,
    pub lens: Vec<u64>,
    /// Memory maps of the files, unless SS_IO=pread (then chunks are read with pread).
    maps: Vec<memmap2::Mmap>,
}

pub struct Chunk {
    pub file: usize,
    pub start: u64,
    pub end: u64,
}

fn list_csv(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".csv") && !name.starts_with('.') && e.path().is_file()
        })
        .map(|e| e.path())
        .collect();
    paths.sort();
    paths
}

impl Input {
    pub fn open(dir: &Path) -> Self {
        let mut files = Vec::new();
        let mut lens = Vec::new();
        for path in list_csv(dir) {
            let file = File::open(&path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display()));
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if len > 0 {
                files.push(file);
                lens.push(len);
            }
        }
        let maps = if std::env::var("SS_IO").as_deref() != Ok("pread") {
            files
                .iter()
                // SAFETY: the inputs are read-only and nothing modifies them while we run.
                .map(|f| unsafe { memmap2::Mmap::map(f) }.expect("mmap failed"))
                .collect()
        } else {
            Vec::new()
        };
        Self { files, lens, maps }
    }

    /// Calls `f` with the fields of every well-formed line of the chunk.
    #[inline]
    pub fn parse<const N: usize, F: FnMut(&[&[u8]; N])>(&self, chunk: &Chunk, buf: &mut Vec<u8>, simd: bool, f: &mut F) {
        if let Some(map) = self.maps.get(chunk.file) {
            if *POPULATE {
                populate(&map[chunk.start as usize..chunk.end as usize]);
            }
            range_lines::<N, F>(map, chunk.start as usize, chunk.end as usize, map.len(), simd, f);
        } else {
            let n = self.read(chunk, buf);
            range_lines::<N, F>(buf, 0, n, n, simd, f);
        }
    }

    pub fn bytes(&self) -> u64 {
        self.lens.iter().sum()
    }

    /// The position just after the first `\n` at or after `from`, or the file length.
    fn next_line_start(&self, file: usize, from: u64) -> u64 {
        let len = self.lens[file];
        let mut buf = vec![0u8; 1 << 16];
        let mut at = from;
        while at < len {
            let n = ((len - at) as usize).min(buf.len());
            read_exact_at(&self.files[file], &mut buf[..n], at);
            if let Some(i) = memchr(b'\n', &buf[..n]) {
                return at + i as u64 + 1;
            }
            at += n as u64;
        }
        len
    }

    /// Line-aligned chunks of about `size` bytes covering every data line (the
    /// first line of each file is its header).
    pub fn chunks(&self, size: usize) -> Vec<Chunk> {
        let size = size.max(1) as u64;
        let mut out = Vec::new();
        for file in 0..self.files.len() {
            let len = self.lens[file];
            let mut start = self.next_line_start(file, 0);
            while start < len {
                let target = start + size;
                let end = if target >= len { len } else { self.next_line_start(file, target - 1) };
                out.push(Chunk { file, start, end });
                start = end;
            }
        }
        out
    }

    /// Reads a chunk into `buf` (followed by PAD bytes of `\n`); returns its length.
    pub fn read(&self, chunk: &Chunk, buf: &mut Vec<u8>) -> usize {
        let n = (chunk.end - chunk.start) as usize;
        if buf.len() < n + PAD {
            buf.resize(n + PAD, 0);
        }
        read_exact_at(&self.files[chunk.file], &mut buf[..n], chunk.start);
        buf[n..n + PAD].fill(b'\n');
        n
    }
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) {
    while !buf.is_empty() {
        #[cfg(unix)]
        let r = std::os::unix::fs::FileExt::read_at(file, buf, offset);
        #[cfg(windows)]
        let r = std::os::windows::fs::FileExt::seek_read(file, buf, offset);
        match r {
            Ok(0) => panic!("unexpected end of file"),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => panic!("read failed: {e}"),
        }
    }
}

/// True if the AVX2 splitter can be used (and SS_SCALAR is not set).
pub fn use_simd() -> bool {
    if std::env::var_os("SS_SCALAR").is_some() {
        return false;
    }
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("avx2") && is_x86_feature_detected!("bmi1") && is_x86_feature_detected!("popcnt")
    }
    #[cfg(not(target_arch = "x86_64"))]
    false
}

/// Calls `f` with the fields of every well-formed line starting in `[start, end)`,
/// `start` being a line start and `data_len` the end of the data in `buf`. The
/// AVX2 path reads 128-byte windows wherever they fit inside `buf` (a chunk buffer
/// has PAD bytes of `\n` after its data, so they always fit there).
#[inline]
pub fn range_lines<const N: usize, F: FnMut(&[&[u8]; N])>(
    buf: &[u8],
    start: usize,
    end: usize,
    data_len: usize,
    simd: bool,
    f: &mut F,
) {
    #[cfg(target_arch = "x86_64")]
    if simd {
        // SAFETY: use_simd() checked the CPU features; windows are bounds-checked.
        unsafe { simd_range::<N, F>(buf, start, end, data_len, f) };
        return;
    }
    let _ = simd;
    let mut out: [&[u8]; N] = [&[]; N];
    let mut p = start;
    while p < end {
        p = scalar_line::<N, F>(buf, p, data_len, &mut out, f);
    }
}

/// One line starting at p: calls `f` if it is well formed; returns the next line start.
#[inline]
fn scalar_line<'a, const N: usize, F: FnMut(&[&[u8]; N])>(
    buf: &'a [u8],
    p: usize,
    data_len: usize,
    out: &mut [&'a [u8]; N],
    f: &mut F,
) -> usize {
    let q = memchr(b'\n', &buf[p..data_len]).map_or(data_len, |i| p + i);
    if csv::fields::<N>(&buf[p..q], out) {
        f(out);
    }
    q + 1
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn block_masks(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    let lo = _mm256_loadu_si256(p as *const __m256i);
    let hi = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let mask = |v: __m256i, c: u8| -> u32 { _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32 };
    let n = mask(lo, b'\n') as u64 | (mask(hi, b'\n') as u64) << 32;
    let c = mask(lo, b',') as u64 | (mask(hi, b',') as u64) << 32;
    let q = mask(lo, b'"') as u64 | (mask(hi, b'"') as u64) << 32;
    (n, c, q)
}

/// Bits from `from` (0..=64) upwards.
#[inline(always)]
fn bits_from(from: usize) -> u64 {
    if from >= 64 { 0 } else { !0u64 << from }
}

/// The lines of `[start, end)` from 64-byte blocks: each byte is classified once,
/// then the commas and newlines are walked in order, the line state carrying
/// over from one block to the next. Same rules as csv::fields: exactly N-1 commas,
/// a field is quoted when it is at least 2 bytes long and starts and ends with a
/// quote, and the line is well formed exactly when it holds two quotes per quoted
/// field and no other quote.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi1,popcnt")]
unsafe fn simd_range<const N: usize, F: FnMut(&[&[u8]; N])>(
    buf: &[u8],
    start: usize,
    end: usize,
    data_len: usize,
    f: &mut F,
) {
    let mut out: [&[u8]; N] = [&[]; N];
    let mut line_start = start;
    let mut field_start = start;
    let mut k = 0usize; // fields completed in the current line
    let mut too_many = false;
    let mut quotes = 0u32; // quotes seen in the current line
    let mut quoted = 0u32; // quoted fields in the current line
    let mut block = start;
    while line_start < end {
        // Masks of this block; past the data (or the buffer) every byte is '\n'.
        let (nl, comma, quote) = if block + 64 <= data_len.min(buf.len()) {
            block_masks(buf.as_ptr().add(block))
        } else {
            let mut tail = [b'\n'; 64];
            let have = data_len.saturating_sub(block).min(64);
            tail[..have].copy_from_slice(&buf[block..block + have]);
            block_masks(tail.as_ptr())
        };
        // Only bytes at or after the current line start count.
        let skip = bits_from(line_start.saturating_sub(block));
        let mut structural = (nl | comma) & skip;
        let mut counted = line_start.saturating_sub(block); // quotes counted up to here
        while structural != 0 {
            let i = structural.trailing_zeros() as usize;
            structural &= structural - 1;
            let pos = block + i;
            if (nl >> i) & 1 == 0 {
                // a comma ends field k
                if k + 1 < N {
                    let field = buf.get_unchecked(field_start..pos);
                    let w = field.len();
                    *out.get_unchecked_mut(k) = if w >= 2 && field[0] == b'"' && field[w - 1] == b'"' {
                        quoted += 1;
                        buf.get_unchecked(field_start + 1..pos - 1)
                    } else {
                        field
                    };
                    k += 1;
                } else {
                    too_many = true;
                }
                field_start = pos + 1;
                continue;
            }
            // a newline (or the end of the data) ends the line
            quotes += (quote & bits_from(counted) & !bits_from(i)).count_ones();
            counted = i + 1;
            let line_end = pos.min(data_len);
            if !too_many && k + 1 == N && line_end > line_start {
                let field = buf.get_unchecked(field_start..line_end);
                let w = field.len();
                *out.get_unchecked_mut(N - 1) = if w >= 2 && field[0] == b'"' && field[w - 1] == b'"' {
                    quoted += 1;
                    buf.get_unchecked(field_start + 1..line_end - 1)
                } else {
                    field
                };
                if quotes == 2 * quoted {
                    f(&out);
                }
            }
            line_start = pos + 1;
            field_start = pos + 1;
            k = 0;
            too_many = false;
            quotes = 0;
            quoted = 0;
            if line_start >= end {
                return;
            }
        }
        quotes += (quote & bits_from(counted)).count_ones();
        block += 64;
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

    fn collect<const N: usize>(text: &[u8], simd: bool) -> Vec<Vec<Vec<u8>>> {
        let mut buf = text.to_vec();
        buf.extend_from_slice(&[b'\n'; PAD]);
        let mut got = Vec::new();
        let mut f = |fields: &[&[u8]; N]| got.push(fields.iter().map(|x| x.to_vec()).collect());
        range_lines::<N, _>(&buf, 0, text.len(), text.len(), simd, &mut f);
        got
    }

    #[test]
    fn simd_matches_scalar() {
        if !use_simd() {
            return;
        }
        let alphabet = b"ab,,\"\" \r\nx1";
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..20_000 {
            let n = (rng(&mut s) % 400) as usize;
            let mut text: Vec<u8> = (0..n).map(|_| alphabet[(rng(&mut s) % alphabet.len() as u64) as usize]).collect();
            if round % 7 == 0 {
                // long lines too
                text.extend(std::iter::repeat_n(b'z', 150));
                text.extend_from_slice(b",q\n\"a\",\"\"\n");
            }
            assert_eq!(collect::<2>(&text, true), collect::<2>(&text, false), "{:?}", String::from_utf8_lossy(&text));
            // Unpadded, as a memory map is: windows near the end fall back to scalar.
            let mut a = Vec::new();
            let mut b = Vec::new();
            range_lines::<4, _>(&text, 0, text.len(), text.len(), true, &mut |x: &[&[u8]; 4]| a.push(x.map(|y| y.to_vec())));
            range_lines::<4, _>(&text, 0, text.len(), text.len(), false, &mut |x: &[&[u8]; 4]| b.push(x.map(|y| y.to_vec())));
            assert_eq!(a, b);
            assert_eq!(collect::<4>(&text, true), collect::<4>(&text, false));
            assert_eq!(collect::<7>(&text, true), collect::<7>(&text, false));
        }
    }
}
