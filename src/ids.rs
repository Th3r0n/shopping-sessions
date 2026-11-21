//! IDs as 42-bit values.
//!
//! Each ID column has a scheme: a prefix of 0 to 4 bytes, the common prefix of IDs
//! sampled from the data (never hard-coded). A *regular* ID is that prefix followed
//! by 1 to 8 characters in `[0-9a-z]` with no leading `0` (the body `0` alone is
//! fine); its value is the body read in base 36, below 36^8 < 2^42. The generator
//! writes exactly such IDs (a letter and base 36 of a 40-bit number). Every other
//! non-empty ID is *exotic*: interned, its value is `EXOTIC_BASE` + its index, also
//! below 2^42. An empty ID is never encoded (callers test for it first).
//!
//! Byte order: two regular IDs of one scheme compare by `order_key` (the body
//! left-aligned in 8 base 36 digits, then its length); as soon as an exotic ID is
//! involved, the real bytes are compared.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Mutex, OnceLock};

/// 36^8: regular values are below it.
pub const REGULAR_LIMIT: u64 = 2_821_109_907_456;
pub const EXOTIC_BASE: u64 = REGULAR_LIMIT;
pub const VALUE_BITS: u32 = 42;
pub const VALUE_MASK: u64 = (1 << VALUE_BITS) - 1;
/// Order keys of regular IDs are below this; exotic click order keys start here.
pub const EXOTIC_ORDER: u64 = 8 * REGULAR_LIMIT;

const POW36: [u64; 9] = {
    let mut t = [1u64; 9];
    let mut i = 1;
    while i < 9 {
        t[i] = t[i - 1] * 36;
        i += 1;
    }
    t
};

/// Base 36 digit of a byte, 0xff for any other byte.
static DIGIT: [u8; 256] = {
    let mut t = [0xffu8; 256];
    let mut i = 0;
    while i < 10 {
        t[b'0' as usize + i] = i as u8;
        i += 1;
    }
    i = 0;
    while i < 26 {
        t[b'a' as usize + i] = 10 + i as u8;
        i += 1;
    }
    t
};

const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scheme {
    prefix: [u8; 4],
    len: usize,
}

impl Scheme {
    pub const EMPTY: Scheme = Scheme { prefix: [0; 4], len: 0 };

    /// The longest common prefix of the samples, at most 4 bytes and shorter than
    /// the shortest sample (so every sample keeps a body). No samples: no prefix.
    pub fn detect<'a>(samples: impl IntoIterator<Item = &'a [u8]>) -> Scheme {
        let samples: Vec<&[u8]> = samples.into_iter().filter(|s| !s.is_empty()).collect();
        let Some(first) = samples.first() else { return Scheme::EMPTY };
        let shortest = samples.iter().map(|s| s.len()).min().unwrap_or(0);
        let mut len = 0;
        while len < 4 && len + 1 < shortest && samples.iter().all(|s| s[len] == first[len]) {
            len += 1;
        }
        let mut prefix = [0u8; 4];
        prefix[..len].copy_from_slice(&first[..len]);
        Scheme { prefix, len }
    }

    pub fn prefix(&self) -> &[u8] {
        &self.prefix[..self.len]
    }

    /// The value of a regular ID, or None (exotic or empty).
    #[inline(always)]
    pub fn regular(&self, id: &[u8]) -> Option<u64> {
        let n = id.len().wrapping_sub(self.len);
        if n.wrapping_sub(1) >= 8 || id[..self.len] != self.prefix[..self.len] {
            return None;
        }
        let body = &id[self.len..];
        if body[0] == b'0' && n > 1 {
            return None;
        }
        #[cfg(all(target_arch = "x86_64", target_feature = "ssse3", target_feature = "sse4.1"))]
        if (body.as_ptr() as usize & 4095) <= 4096 - 16 {
            // SAFETY: the 16 bytes read stay inside the page of body[0], which is mapped.
            return unsafe { base36_simd(body.as_ptr(), n) };
        }
        base36_scalar(body)
    }

    /// The value of a non-empty ID (exotic IDs are interned).
    #[inline(always)]
    pub fn value(&self, id: &[u8]) -> u64 {
        match self.regular(id) {
            Some(v) => v,
            None => intern(id),
        }
    }

    /// Appends the ID of `value` to `out`.
    pub fn bytes_into(&self, value: u64, out: &mut Vec<u8>) {
        if value < REGULAR_LIMIT {
            out.extend_from_slice(self.prefix());
            let n = digits36(value);
            let at = out.len();
            out.resize(at + n, b'0');
            let mut rest = value;
            for i in (0..n).rev() {
                out[at + i] = DIGITS[(rest % 36) as usize];
                rest /= 36;
            }
        } else {
            exotic_bytes_into(value, out);
        }
    }

    pub fn to_bytes(&self, value: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        self.bytes_into(value, &mut out);
        out
    }

    /// Byte order of the IDs of two values of this scheme.
    #[inline]
    pub fn cmp(&self, a: u64, b: u64) -> Ordering {
        if a < REGULAR_LIMIT && b < REGULAR_LIMIT {
            order_key(a).cmp(&order_key(b))
        } else {
            self.to_bytes(a).cmp(&self.to_bytes(b))
        }
    }

    /// Byte order of two click order keys (see `click_order`).
    #[inline]
    pub fn cmp_click(&self, a: u64, b: u64) -> Ordering {
        if a < EXOTIC_ORDER && b < EXOTIC_ORDER {
            a.cmp(&b)
        } else {
            self.to_bytes(click_value(a)).cmp(&self.to_bytes(click_value(b)))
        }
    }
}

/// Number of base 36 digits of a regular value (at least 1).
#[inline]
fn digits36(v: u64) -> usize {
    let mut n = 1;
    while n < 8 && v >= POW36[n] {
        n += 1;
    }
    n
}

/// A key whose integer order is the byte order of regular IDs of one scheme: the
/// body left-aligned in 8 digits, then its length (a prefix sorts first). Below
/// EXOTIC_ORDER (8 * 36^8 < 2^45).
#[inline]
pub fn order_key(v: u64) -> u64 {
    let n = digits36(v);
    v * POW36[8 - n] * 8 + (n as u64 - 1)
}

/// The order key of a click_id value: `order_key` for a regular ID, EXOTIC_ORDER +
/// index for an exotic one (those compare by bytes). Below 2^45.
#[inline]
pub fn click_order(v: u64) -> u64 {
    if v < REGULAR_LIMIT {
        order_key(v)
    } else {
        EXOTIC_ORDER + (v - EXOTIC_BASE)
    }
}

/// The value back from a click order key.
#[inline]
pub fn click_value(k: u64) -> u64 {
    if k >= EXOTIC_ORDER {
        EXOTIC_BASE + (k - EXOTIC_ORDER)
    } else {
        let n = (k & 7) as usize + 1;
        (k >> 3) / POW36[8 - n]
    }
}

#[inline]
fn base36_scalar(body: &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for &c in body {
        let d = DIGIT[c as usize];
        if d == 0xff {
            return None;
        }
        v = v * 36 + d as u64;
    }
    Some(v)
}

/// For n body bytes: moves byte i to lane 8 - n + i (lanes 0..8 - n and 8.. zeroed).
#[cfg(all(target_arch = "x86_64", target_feature = "ssse3", target_feature = "sse4.1"))]
static ALIGN: [[i8; 16]; 9] = {
    let mut t = [[-1i8; 16]; 9];
    let mut n = 1;
    while n <= 8 {
        let mut lane = 8 - n;
        while lane < 8 {
            t[n][lane] = (lane - (8 - n)) as i8;
            lane += 1;
        }
        n += 1;
    }
    t
};

/// base36_scalar on up to 8 bytes at once: digit values by range, right-aligned into
/// 8 lanes, then pairs (x36) and quads (x1296) with maddubs / madd.
#[cfg(all(target_arch = "x86_64", target_feature = "ssse3", target_feature = "sse4.1"))]
#[inline(always)]
unsafe fn base36_simd(p: *const u8, n: usize) -> Option<u64> {
    use std::arch::x86_64::*;
    let c = _mm_loadu_si128(p as *const __m128i);
    let iota = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let inside = _mm_cmpgt_epi8(_mm_set1_epi8(n as i8), iota);
    let in_range = |lo: u8, hi: u8| _mm_and_si128(_mm_cmpgt_epi8(c, _mm_set1_epi8(lo as i8 - 1)), _mm_cmplt_epi8(c, _mm_set1_epi8(hi as i8 + 1)));
    let digit = in_range(b'0', b'9');
    let lower = in_range(b'a', b'z');
    if _mm_movemask_epi8(_mm_andnot_si128(_mm_or_si128(digit, lower), inside)) != 0 {
        return None;
    }
    let offset = _mm_or_si128(_mm_and_si128(digit, _mm_set1_epi8(48)), _mm_and_si128(lower, _mm_set1_epi8(87)));
    let d = _mm_sub_epi8(c, offset);
    let d = _mm_shuffle_epi8(d, _mm_loadu_si128(ALIGN[n].as_ptr() as *const __m128i));
    let pairs = _mm_maddubs_epi16(d, _mm_setr_epi8(36, 1, 36, 1, 36, 1, 36, 1, 0, 0, 0, 0, 0, 0, 0, 0));
    let quads = _mm_madd_epi16(pairs, _mm_setr_epi16(1296, 1, 1296, 1, 0, 0, 0, 0));
    let hi = _mm_cvtsi128_si32(quads) as u32 as u64;
    let lo = _mm_extract_epi32::<1>(quads) as u32 as u64;
    Some(hi * 1_679_616 + lo)
}

struct Interner {
    index: HashMap<Box<[u8]>, u32>,
    bytes: Vec<Box<[u8]>>,
}

fn interner() -> &'static Mutex<Interner> {
    static INTERNER: OnceLock<Mutex<Interner>> = OnceLock::new();
    INTERNER.get_or_init(|| Mutex::new(Interner { index: HashMap::new(), bytes: Vec::new() }))
}

static EXOTICS: AtomicUsize = AtomicUsize::new(0);

/// How many distinct exotic IDs were interned so far.
pub fn exotic_count() -> usize {
    EXOTICS.load(AtomicOrdering::Relaxed)
}

#[cold]
#[inline(never)]
fn intern(bytes: &[u8]) -> u64 {
    assert!(!bytes.is_empty(), "an empty ID is never interned");
    let mut guard = interner().lock().unwrap();
    if let Some(&i) = guard.index.get(bytes) {
        return EXOTIC_BASE + i as u64;
    }
    let i = guard.bytes.len() as u32;
    let boxed: Box<[u8]> = bytes.into();
    guard.bytes.push(boxed.clone());
    guard.index.insert(boxed, i);
    EXOTICS.store(guard.bytes.len(), AtomicOrdering::Relaxed);
    EXOTIC_BASE + i as u64
}

fn exotic_bytes_into(value: u64, out: &mut Vec<u8>) {
    let guard = interner().lock().unwrap();
    out.extend_from_slice(&guard.bytes[(value - EXOTIC_BASE) as usize]);
}

/// Multiplicative hash of a value: use its high bits.
#[inline(always)]
pub fn hash(v: u64) -> u64 {
    v.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// A second, independent hash.
#[inline(always)]
pub fn hash2(v: u64) -> u64 {
    (v ^ 0x5bd1_e995_3c6e_f372).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
}

const M1: u64 = 0x2F3_7A1B_5C9D & VALUE_MASK | 1;
const M2: u64 = 0x1D8_E4F2_A6B7 & VALUE_MASK | 1;

const fn inverse(m: u64) -> u64 {
    // Newton: x <- x (2 - m x) doubles the correct low bits each step.
    let mut x = m;
    let mut i = 0;
    while i < 6 {
        x = x.wrapping_mul(2u64.wrapping_sub(m.wrapping_mul(x)));
        i += 1;
    }
    x
}
const M1_INV: u64 = inverse(M1);
const M2_INV: u64 = inverse(M2);

/// A bijection of 42-bit values that spreads them: its low bits depend on every
/// bit of the value, so they pick partitions evenly whatever the IDs look like.
#[inline(always)]
pub fn mix(v: u64) -> u64 {
    let mut x = v.wrapping_mul(M1) & VALUE_MASK;
    x ^= x >> 21;
    x.wrapping_mul(M2) & VALUE_MASK
}

#[inline(always)]
pub fn unmix(x: u64) -> u64 {
    let mut x = x.wrapping_mul(M2_INV) & VALUE_MASK;
    x ^= x >> 21;
    x.wrapping_mul(M1_INV) & VALUE_MASK
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

    fn scheme(prefix: &[u8]) -> Scheme {
        let mut p = [0u8; 4];
        p[..prefix.len()].copy_from_slice(prefix);
        Scheme { prefix: p, len: prefix.len() }
    }

    /// Bodies over [0-9a-z] (all of length 1 and 2, a sample of 3), canonical or not,
    /// plus long, uppercase and odd ones.
    fn ids(prefix: &[u8]) -> Vec<Vec<u8>> {
        let alphabet: Vec<u8> = DIGITS.to_vec();
        let mut out = Vec::new();
        for &a in &alphabet {
            out.push([prefix, &[a]].concat());
            for &b in &alphabet {
                out.push([prefix, &[a, b]].concat());
            }
        }
        let mut s = 0x1234_5678_9abc_def1u64;
        for _ in 0..4000 {
            let len = 3 + (rng(&mut s) % 8) as usize; // 3 to 10
            let body: Vec<u8> = (0..len).map(|_| alphabet[(rng(&mut s) % 36) as usize]).collect();
            out.push([prefix, &body].concat());
        }
        for odd in [&b""[..], b"0", b"00", b"0a", b"A", b"U1", b"zzzzzzzz", b"zzzzzzzzz", b"10000000", b"100000000", b" 1", b"1 ", b"1\r", b"a-b", b"\xc3\xa9"] {
            out.push([prefix, odd].concat());
        }
        out.push(b"x".to_vec());
        out.push(b"".to_vec());
        out.push(b"A\tB".to_vec());
        out.retain(|id| !id.is_empty());
        out
    }

    #[test]
    fn values_round_trip_and_keep_byte_order() {
        for prefix in [&b""[..], b"u", b"c_", b"abcd"] {
            let sc = scheme(prefix);
            let ids = ids(prefix);
            let values: Vec<u64> = ids.iter().map(|id| sc.value(id)).collect();
            for (id, &v) in ids.iter().zip(&values) {
                assert!(v <= VALUE_MASK);
                assert_eq!(&sc.to_bytes(v), id, "{:?}", String::from_utf8_lossy(id));
                let body = &id[prefix.len().min(id.len())..];
                let canonical = id.starts_with(prefix) && (1..=8).contains(&body.len()) && body.iter().all(|c| DIGITS.contains(c)) && (body[0] != b'0' || body.len() == 1);
                assert_eq!(v < REGULAR_LIMIT, canonical, "{:?}", String::from_utf8_lossy(id));
                assert_eq!(unmix(mix(v)), v);
                assert_eq!(click_value(click_order(v)), v);
            }
            let mut s = 99u64;
            for _ in 0..200_000 {
                let i = (rng(&mut s) % ids.len() as u64) as usize;
                let j = (rng(&mut s) % ids.len() as u64) as usize;
                let want = ids[i].cmp(&ids[j]);
                assert_eq!(sc.cmp(values[i], values[j]), want, "{:?} {:?}", String::from_utf8_lossy(&ids[i]), String::from_utf8_lossy(&ids[j]));
                assert_eq!(sc.cmp_click(click_order(values[i]), click_order(values[j])), want);
                assert_eq!(values[i] == values[j], ids[i] == ids[j]);
            }
        }
    }

    #[test]
    fn simd_matches_scalar_everywhere() {
        // IDs that start anywhere, including the last bytes of a page
        let sc = scheme(b"u");
        let mut buf = vec![b'0'; 3 * 4096];
        let mut s = 0x0123_4567_89ab_cdefu64;
        let alphabet = b"0123456789abcxyzAZ_ /:@[`{\x00\x7f\x80\xff";
        for b in buf.iter_mut() {
            *b = alphabet[(rng(&mut s) % alphabet.len() as u64) as usize];
        }
        for _ in 0..300_000 {
            let start = (rng(&mut s) % (buf.len() as u64 - 16)) as usize;
            let len = (rng(&mut s) % 11) as usize;
            buf[start] = b'u';
            if rng(&mut s) % 2 == 0 {
                for b in &mut buf[start + 1..start + 1 + len] {
                    *b = DIGITS[(rng(&mut s) % 36) as usize];
                }
            }
            let id = &buf[start..start + 1 + len];
            let body = &id[1..];
            let want = if (1..=8).contains(&body.len()) && (body[0] != b'0' || body.len() == 1) { base36_scalar(body) } else { None };
            assert_eq!(sc.regular(id), want, "{:?}", id);
        }
    }

    #[test]
    fn scheme_detection() {
        assert_eq!(Scheme::detect([&b"e42huhoug"[..], b"ec1k0erc1", b"e1861btom"]).prefix(), b"e");
        assert_eq!(Scheme::detect([&b"e1"[..], b"e10", b"e11"]).prefix(), b"e");
        assert_eq!(Scheme::detect([&b"c_1"[..], b"c_2", b"c_33"]).prefix(), b"c_");
        assert_eq!(Scheme::detect([&b"u1"[..], b"A\tB", b"u2"]).prefix(), b"");
        assert_eq!(Scheme::detect(std::iter::empty::<&[u8]>()).prefix(), b"");
        assert_eq!(Scheme::detect([&b"x"[..]]).prefix(), b"");
    }

    #[test]
    fn generator_ids() {
        let sc = scheme(b"u");
        assert_eq!(sc.regular(b"uduffvac"), Some(base36_scalar(b"duffvac").unwrap()));
        assert_eq!(sc.regular(b"u0"), Some(0));
        assert_eq!(sc.regular(b"u00"), None);
        assert_eq!(sc.regular(b"u"), None);
        assert_eq!(sc.regular(b"U1"), None);
        assert_eq!(sc.regular(b"uzzzzzzzz"), Some(REGULAR_LIMIT - 1));
        assert_eq!(sc.regular(b"u100000000"), None);
        // k0 < k0b (a prefix sorts first) although both bodies start with 0
        let k = scheme(b"k");
        assert_eq!(k.cmp(k.value(b"k0"), k.value(b"k0b")), Ordering::Less);
        assert_eq!(k.cmp_click(click_order(k.value(b"K5")), click_order(k.value(b"k5b"))), Ordering::Less);
    }
}
