//! Event and click lines to records.

use crate::arena::{Scatter, Words3};
use crate::hash::{bucket, mix};
use crate::key;
use crate::links::Names;
use crate::sessions::PersonClick;
use crate::lookups::{Campaigns, Products};
use crate::ts::{self, DAY};

/// The packed word of an event: price (bits 0-23), category (24-28), flags (29-31)
/// and ts relative to `Context::base` (32-63), so that (person, packed) sorts by
/// person then ts. main checks at startup that every in-window ts fits in 32 bits
/// and every category id in 5; otherwise the generic binary runs instead.
pub const SURVIVES: u64 = 1 << 29;
pub const PURCHASE: u64 = 1 << 30;
pub const MAX_CATEGORIES: usize = 32;

/// The copies of a thread, in two scatters over the same event_id partitions:
/// most in 3 words (`compact`), the rest as an EventCopy (`full`).
///
/// A compact copy is [event_id, user, ingest_s << 32 | C_SURVIVES | ts_rel]:
/// ingest_ts in whole seconds after `Context::base` (as an i32), ts relative to
/// base in 31 bits (main checks the window allows it). It holds a copy that would
/// not survive (user and ts 0), or one that would and is not a purchase. Copies
/// whose ingest_ts does not fit, and surviving purchases (price and category),
/// are full.
pub struct Copies {
    pub compact: Words3,
    pub full: Scatter<EventCopy>,
}

pub const C_SURVIVES: u64 = 1 << 31;
pub const C_TS: u64 = (1 << 31) - 1;

#[inline]
pub fn pack(ts_rel: u32, price: u32, category: u16, flags: u64) -> u64 {
    (ts_rel as u64) << 32 | price as u64 | (category as u64) << 24 | flags
}
#[inline]
pub fn ts_of(packed: u64) -> i64 {
    (packed >> 32) as i64
}
#[inline]
pub fn price_of(packed: u64) -> u32 {
    packed as u32 & 0x00ff_ffff
}
#[inline]
pub fn category_of(packed: u64) -> u16 {
    (packed >> 24) as u16 & 31
}
pub const WEEK: i64 = 7 * DAY;

/// One well-formed copy of an event with a non-empty event_id. `flags` says whether
/// the copy would survive validation and the late check if it is the kept copy; the
/// other fields only matter when it survives.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct EventCopy {
    pub event_id: u64,
    pub ingest: i64,
    pub user: u64,
    /// See `pack`; 0 (no flags) for a copy that would not survive.
    pub packed: u64,
}

/// A short FIFO of values whose name lookups were prefetched: a value comes out
/// LAG pushes after it went in, by which time its cache line has arrived.
pub struct Pending<T: Copy + Default, const N: usize = LAG> {
    buf: [T; N],
    head: usize,
    len: usize,
}

const LAG: usize = 16;

impl<T: Copy + Default, const N: usize> Pending<T, N> {
    pub fn new() -> Self {
        Self { buf: [T::default(); N], head: 0, len: 0 }
    }

    /// Adds `value`; returns the oldest value once N are waiting.
    #[inline]
    pub fn push(&mut self, value: T) -> Option<T> {
        if self.len < N {
            self.buf[(self.head + self.len) % N] = value;
            self.len += 1;
            None
        } else {
            let out = self.buf[self.head];
            self.buf[self.head] = value;
            self.head = (self.head + 1) % N;
            Some(out)
        }
    }

    /// The value pushed `k` pushes before the newest one, if still waiting.
    #[inline]
    pub fn back(&self, k: usize) -> Option<&T> {
        if k < self.len {
            Some(&self.buf[(self.head + self.len - 1 - k) % N])
        } else {
            None
        }
    }

    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let out = self.buf[self.head];
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(out)
    }
}

/// Clicks wait 2 LAG pushes: the filter word is prefetched when a click goes in,
/// its table slot (only if the filter does not clear it) LAG pushes later.
pub type ClickPending = Pending<PersonClick, { 2 * LAG }>;

pub struct Context<'a> {
    /// window_start - 7 days: every kept ts is stored relative to it.
    pub base: i64,
    pub window_start: i64,
    pub window_end: i64,
    pub products: &'a Products,
    pub campaigns: &'a Campaigns,
}

/// `price_cents` of a purchase: ASCII digits only, 1 to 10,000,000.
#[inline]
fn price(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let first = b.iter().position(|&c| c != b'0')?;
    let digits = &b[first..];
    if digits.len() > 8 {
        return None;
    }
    let value = digits.iter().fold(0u32, |v, &c| v * 10 + (c - b'0') as u32);
    (1..=10_000_000).contains(&value).then_some(value)
}

#[inline]
pub fn event_fields(f: &[&[u8]; 7], cx: &Context, parts: &mut Copies) {
    let [event_id, user, ts, ingest, event_type, product, price_cents] = *f;
    if event_id.is_empty() {
        return;
    }
    let mut copy = EventCopy {
        event_id: key::key(event_id),
        ingest: ts::ingest(ingest),
        user: 0,
        packed: 0,
    };
    'valid: {
        if user.is_empty() || product.is_empty() {
            break 'valid;
        }
        // view and cart are compared as words, without a branch between them
        let purchase = match *event_type {
            [a, b, c, d] => {
                let w = u32::from_le_bytes([a, b, c, d]);
                if (w == u32::from_le_bytes(*b"view")) | (w == u32::from_le_bytes(*b"cart")) {
                    false
                } else {
                    break 'valid;
                }
            }
            [a, b, c, d, e, f, g, h] if u64::from_le_bytes([a, b, c, d, e, f, g, h]) == u64::from_le_bytes(*b"purchase") => true,
            _ => break 'valid,
        };
        let user_key = key::key(user);
        let t = match ts::parse(ts) {
            Some(t) if t >= cx.window_start && t < cx.window_end => t,
            _ => break 'valid,
        };
        let (mut cents, mut category) = (0, 0);
        if purchase {
            match price(price_cents) {
                Some(p) => cents = p,
                None => break 'valid,
            }
            category = cx.products.category(product);
        }
        if copy.ingest.saturating_sub(t) > DAY {
            break 'valid;
        }
        let flags = SURVIVES | if purchase { PURCHASE } else { 0 };
        copy.packed = pack((t - cx.base) as u32, cents, category, flags);
        copy.user = user_key;
    }
    let p = bucket(mix(copy.event_id), parts.full.parts());
    let rel = copy.ingest.wrapping_sub(cx.base);
    let seconds = rel / 1000;
    if rel % 1000 == 0 && seconds == seconds as i32 as i64 && copy.packed & PURCHASE == 0 {
        let survives = copy.packed & SURVIVES != 0;
        let word = (seconds as i32 as u32 as u64) << 32
            | if survives { C_SURVIVES | ts_of(copy.packed) as u64 } else { 0 };
        parts.compact.push(p, [copy.event_id, copy.user, word]);
    } else {
        parts.full.push_wc(p, copy);
    }
}


#[inline]
pub fn click_fields(
    f: &[&[u8]; 4],
    cx: &Context,
    names: &Names,
    pending: &mut ClickPending,
    out: &mut Words3,
) {
    let [click_id, user, campaign, ts] = *f;
    if click_id.is_empty() || user.is_empty() {
        return;
    }
    let user_key = key::key(user);
    names.prefetch_filter(user_key);
    let t = match ts::parse(ts) {
        Some(t) if t >= cx.window_start.saturating_sub(WEEK) && t < cx.window_end => t,
        _ => return,
    };
    let Some(campaign) = cx.campaigns.get(campaign) else {
        return;
    };
    let click = PersonClick {
        person: user_key,
        click_id: key::key(click_id),
        ts: (t - cx.base) as u32,
        campaign,
    };
    if let Some(mut old) = pending.push(click) {
        old.person = names.get(old.person);
        out.push(bucket(mix(old.person), out.parts()), old.words());
    }
    if let Some(mid) = pending.back(LAG) {
        names.prefetch_if_renamed(mid.person);
    }
}

/// Resolves and stores the clicks still waiting in `pending`.
pub fn flush_clicks(names: &Names, pending: &mut ClickPending, out: &mut Words3) {
    while let Some(mut old) = pending.pop() {
        old.person = names.get(old.person);
        out.push(bucket(mix(old.person), out.parts()), old.words());
    }
}

#[inline]
pub fn link_fields(f: &[&[u8]; 2], out: &mut Vec<(u64, u64)>) {
    let [a, b] = *f;
    if a.is_empty() || b.is_empty() {
        return;
    }
    out.push((key::key(a), key::key(b)));
}

#[cfg(test)]
mod tests {
    use super::price;

    #[test]
    fn prices() {
        for (s, v) in [("1", Some(1)), ("0250", Some(250)), ("10000000", Some(10_000_000)), ("0000000000010000000", Some(10_000_000))] {
            assert_eq!(price(s.as_bytes()), v, "{s}");
        }
        for s in ["", "0", "00", "10000001", "250000000", "19.99", "-5", "+500", "1e3", " 1", "1\r", "٢٥٠", "２５０"] {
            assert_eq!(price(s.as_bytes()), None, "{s:?}");
        }
    }
}
