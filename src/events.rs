//! Event and click lines to records.

use crate::csv;
use crate::hash::{bucket, mix};
use crate::key;
use crate::lookups::{Campaigns, Products};
use crate::ts::{self, DAY};

pub const SURVIVES: u8 = 1;
pub const PURCHASE: u8 = 2;
pub const WEEK: i64 = 7 * DAY;

/// One well-formed copy of an event with a non-empty event_id. `flags` says whether
/// the copy would survive validation and the late check if it is the kept copy; the
/// other fields only matter when it survives.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct EventCopy {
    pub event_id: u64,
    pub ingest: i64,
    pub user: u64,
    pub ts: i64,
    pub price: u32,
    pub category: u16,
    pub flags: u8,
}

/// A valid click, by user.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Click {
    pub user: u64,
    pub click_id: u64,
    pub ts: i64,
    pub campaign: u32,
}

pub struct Context<'a> {
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
pub fn event_line(line: &[u8], cx: &Context, parts: &mut [Vec<EventCopy>]) {
    let mut f = [&b""[..]; 7];
    if !csv::fields::<7>(line, &mut f) {
        return;
    }
    let [event_id, user, ts, ingest, event_type, product, price_cents] = f;
    if event_id.is_empty() {
        return;
    }
    let mut copy = EventCopy {
        event_id: key::key(event_id),
        ingest: ts::parse(ingest).unwrap_or(i64::MAX),
        user: 0,
        ts: 0,
        price: 0,
        category: 0,
        flags: 0,
    };
    'valid: {
        if user.is_empty() || product.is_empty() {
            break 'valid;
        }
        let t = match ts::parse(ts) {
            Some(t) if t >= cx.window_start && t < cx.window_end => t,
            _ => break 'valid,
        };
        let purchase = match event_type {
            b"view" | b"cart" => false,
            b"purchase" => true,
            _ => break 'valid,
        };
        if purchase {
            match price(price_cents) {
                Some(p) => copy.price = p,
                None => break 'valid,
            }
            copy.category = cx.products.category(product);
        }
        if copy.ingest.saturating_sub(t) > DAY {
            break 'valid;
        }
        copy.user = key::key(user);
        copy.ts = t;
        copy.flags = SURVIVES | if purchase { PURCHASE } else { 0 };
    }
    let p = bucket(mix(copy.event_id), parts.len());
    parts[p].push(copy);
}

#[inline]
pub fn click_line(line: &[u8], cx: &Context, out: &mut Vec<Click>) {
    let mut f = [&b""[..]; 4];
    if !csv::fields::<4>(line, &mut f) {
        return;
    }
    let [click_id, user, campaign, ts] = f;
    if click_id.is_empty() || user.is_empty() {
        return;
    }
    let t = match ts::parse(ts) {
        Some(t) if t >= cx.window_start.saturating_sub(WEEK) && t < cx.window_end => t,
        _ => return,
    };
    let Some(campaign) = cx.campaigns.get(campaign) else {
        return;
    };
    out.push(Click {
        user: key::key(user),
        click_id: key::key(click_id),
        ts: t,
        campaign,
    });
}

#[inline]
pub fn link_line(line: &[u8], out: &mut Vec<(u64, u64)>) {
    let mut f = [&b""[..]; 2];
    if !csv::fields::<2>(line, &mut f) {
        return;
    }
    let [a, b] = f;
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
