//! Dedup of event copies, then per person: sessions, credit, assists and the daily
//! aggregates.

use std::cmp::Ordering;

use crate::arena::Scatter;
use crate::events::{category_of, price_of, ts_of, EventCopy, PURCHASE, RESOLVED, SURVIVES, WEEK};
use crate::hash::{bucket, mix};
use crate::key;
use crate::links::Names;
use crate::ts::DAY;

pub const GAP: i64 = 1_800_000;
pub const NO_CATEGORY: u16 = u16::MAX;

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct PersonEvent {
    pub person: u64,
    /// As in EventCopy (see events::pack).
    pub packed: u64,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct PersonClick {
    pub person: u64,
    pub click_id: u64,
    /// Relative to Context::base.
    pub ts: u32,
    pub campaign: u32,
}

/// A dedup table reused across partitions without clearing it. A slot packs the
/// kept copy's position (bits 0..32), 24 bits of its event_id's hash (32..56) and
/// the partition's generation (56..64): a slot of another generation is empty.
#[derive(Default)]
pub struct Table {
    slots: Vec<u64>,
    generation: u64,
    /// One bit per copy of the partition: set when another copy wins.
    lost: Vec<u64>,
    /// The dense number of each slice's first copy.
    first: Vec<u32>,
}

/// Keeps, per event_id, the copy with the smallest ingest_ts (copies tied on it are
/// identical) and sends the kept copies that survive to their person's partition.
/// `parts` holds this event_id partition's copies, one slice per parsing thread.
///
/// The first pass inserts every copy into a small table and marks the copies that
/// lose; the second reads the copies again in order, skipping the losers.
#[inline(never)]
pub fn dedup(
    parts: &[&[EventCopy]],
    names: &Names,
    table: &mut Table,
    kept: &mut Vec<PersonEvent>,
    out: &mut Scatter<PersonEvent>,
) {
    let n: usize = parts.iter().map(|p| p.len()).sum();
    if n == 0 {
        return;
    }
    assert!(n < u32::MAX as usize, "partition too large");
    // A position packs the slice index in its high bits and the offset in the rest;
    // `first[k]` is the dense number of slice k's first copy.
    let slice_bits = usize::BITS - (parts.len().max(2) - 1).leading_zeros();
    let shift = 32 - slice_bits;
    assert!(parts.iter().all(|p| (p.len() as u64) < (1u64 << shift)), "partition too large");
    let offset_mask = (1u64 << shift) - 1;
    let copy_at = |at: u64| -> &EventCopy { &parts[(at >> shift) as usize][(at & offset_mask) as usize] };
    table.first.clear();
    let mut acc = 0u32;
    for part in parts {
        table.first.push(acc);
        acc += part.len() as u32;
    }
    let first = &table.first;
    let dense = |at: u64| -> usize { first[(at >> shift) as usize] as usize + (at & offset_mask) as usize };

    let size = (n * 2).next_power_of_two();
    let mask = size - 1;
    if table.slots.len() < size {
        table.slots.resize(size, 0);
    }
    table.generation += 1;
    if table.generation == 256 {
        // the 8-bit generation wrapped: start over from a clean table
        table.slots.iter_mut().for_each(|s| *s = 0);
        table.generation = 1;
    }
    let generation = table.generation << 56;
    let lost_words = n.div_ceil(64);
    table.lost.clear();
    table.lost.resize(lost_words, 0);
    let lost = &mut table.lost;
    let slots = &mut table.slots[..size];
    for (k, part) in parts.iter().enumerate() {
        for (o, c) in part.iter().enumerate() {
            let at = ((k as u64) << shift) | o as u64;
            let h = mix(c.event_id);
            let tag = (h >> 40) << 32; // 24 bits, above the position
            let mut i = h as usize & mask;
            loop {
                let s = slots[i];
                if s >> 56 << 56 != generation {
                    slots[i] = generation | tag | at;
                    break;
                }
                if s & (0xff_ffff << 32) == tag {
                    let old = s & 0xffff_ffff;
                    let other = copy_at(old);
                    if other.event_id == c.event_id {
                        // A duplicate (rare): the smaller ingest_ts wins; ties are identical copies.
                        let loser = if c.ingest < other.ingest {
                            slots[i] = generation | tag | at;
                            old
                        } else {
                            at
                        };
                        let d = dense(loser);
                        lost[d / 64] |= 1 << (d % 64);
                        break;
                    }
                }
                i = (i + 1) & mask;
            }
        }
    }
    // The kept copies that survive, in order, then their names: the name lookups
    // miss the cache, so each one is prefetched LAG entries ahead.
    kept.clear();
    for (k, part) in parts.iter().enumerate() {
        let base = first[k] as usize;
        for (o, c) in part.iter().enumerate() {
            let d = base + o;
            if c.packed & SURVIVES == 0 || (lost[d / 64] >> (d % 64)) & 1 != 0 {
                continue;
            }
            let e = PersonEvent {
                person: c.user,
                packed: c.packed,
            };
            if c.packed & RESOLVED != 0 {
                out.push_wc(bucket(mix(e.person), out.parts()), e);
            } else {
                kept.push(e);
            }
        }
    }
    // Two prefetch steps: the filter word 2 LAG ahead, then (only for the keys the
    // filter does not clear) the table slot LAG ahead.
    for e in kept.iter().take(2 * LAG) {
        names.prefetch_filter(e.person);
    }
    for e in kept.iter().take(LAG) {
        names.prefetch_if_renamed(e.person);
    }
    for i in 0..kept.len() {
        if let Some(far) = kept.get(i + 2 * LAG) {
            names.prefetch_filter(far.person);
        }
        if let Some(ahead) = kept.get(i + LAG) {
            names.prefetch_if_renamed(ahead.person);
        }
        let mut e = kept[i];
        e.person = names.get(e.person);
        out.push_wc(bucket(mix(e.person), out.parts()), e);
    }
}

const LAG: usize = 16;

#[derive(Clone, Copy)]
pub struct Shape {
    /// Context::base: ts values in records are relative to it.
    pub base: i64,
    pub first_day: i64,
    pub days: usize,
    pub campaigns: usize,
}

/// One thread's share of the output.
pub struct Acc {
    pub user: Vec<u64>,
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub events: Vec<i64>,
    pub revenue: Vec<i64>,
    pub top: Vec<u16>,
    /// `[(campaign + 1) * days + day]`: credited sessions, purchasing sessions,
    /// revenue, assisted sessions. Index 0 is `organic`.
    pub stats: Vec<[i64; 4]>,
    shape: Shape,
    count: Vec<u32>,
    pos: Vec<u32>,
    active: Vec<u32>,
    categories: Vec<(u16, i64)>,
    evs: Vec<PersonEvent>,
    cls: Vec<PersonClick>,
    /// Radix sort scratch, the persons by rank and the rank table.
    evs_tmp: Vec<PersonEvent>,
    cls_tmp: Vec<PersonClick>,
    persons: Vec<u64>,
    ranks: Ranks,
}

impl Acc {
    /// `rows`: about how many user_daily rows this thread will produce.
    pub fn new(shape: Shape, rows: usize) -> Self {
        use crate::arena::huge_vec;
        Self {
            user: huge_vec(rows),
            day: huge_vec(rows),
            sessions: huge_vec(rows),
            events: huge_vec(rows),
            revenue: huge_vec(rows),
            top: huge_vec(rows),
            stats: vec![[0; 4]; (shape.campaigns + 1) * shape.days],
            shape,
            count: vec![0; shape.campaigns],
            pos: vec![u32::MAX; shape.campaigns],
            active: Vec::new(),
            categories: Vec::new(),
            evs: Vec::new(),
            cls: Vec::new(),
            evs_tmp: Vec::new(),
            cls_tmp: Vec::new(),
            persons: Vec::new(),
            ranks: Ranks::default(),
        }
    }

    /// Processes one person partition, given as one slice per thread.
    ///
    /// Each person gets a dense rank (in order of first event); events, then
    /// clicks, are radix-sorted by (rank, ts), so each person's events and clicks
    /// form one run in ts order. Clicks of a person without events are dropped.
    #[inline(never)]
    pub fn partition<'a>(
        &mut self,
        evs: impl Iterator<Item = &'a [PersonEvent]>,
        cls: impl Iterator<Item = &'a [PersonClick]>,
    ) {
        let mut e = std::mem::take(&mut self.evs);
        let mut c = std::mem::take(&mut self.cls);
        let mut persons = std::mem::take(&mut self.persons);
        e.clear();
        c.clear();
        persons.clear();
        self.ranks.reset();
        for part in evs {
            for x in part {
                let rank = self.ranks.rank(x.person, &mut persons);
                e.push(PersonEvent { person: rank as u64, packed: x.packed });
            }
        }
        for part in cls {
            for x in part {
                if let Some(rank) = self.ranks.find(x.person) {
                    c.push(PersonClick { person: rank as u64, ..*x });
                }
            }
        }
        let rank_bits = u64::BITS - (persons.len() as u64).leading_zeros();
        // ts sits in the high 32 bits of packed: the key is (rank, ts).
        radix_sort(&mut e, &mut self.evs_tmp, 32 + rank_bits, |x| x.person << 32 | x.packed >> 32);
        radix_sort(&mut c, &mut self.cls_tmp, 32 + rank_bits, |x| x.person << 32 | x.ts as u64);
        let (mut i, mut ci) = (0usize, 0usize);
        while i < e.len() {
            let rank = e[i].person;
            let mut j = i + 1;
            while j < e.len() && e[j].person == rank {
                j += 1;
            }
            while ci < c.len() && c[ci].person < rank {
                ci += 1;
            }
            let mut cj = ci;
            while cj < c.len() && c[cj].person == rank {
                cj += 1;
            }
            self.person(persons[rank as usize], &e[i..j], &c[ci..cj]);
            i = j;
            ci = cj;
        }
        self.evs = e;
        self.cls = c;
        self.persons = persons;
    }

    #[inline]
    fn enter(&mut self, campaign: u32) {
        let c = campaign as usize;
        if self.count[c] == 0 {
            self.pos[c] = self.active.len() as u32;
            self.active.push(campaign);
        }
        self.count[c] += 1;
    }

    #[inline]
    fn leave(&mut self, campaign: u32) {
        let c = campaign as usize;
        self.count[c] -= 1;
        if self.count[c] == 0 {
            let p = self.pos[c] as usize;
            let last = self.active.pop().unwrap();
            if last != campaign {
                self.active[p] = last;
                self.pos[last as usize] = p as u32;
            }
            self.pos[c] = u32::MAX;
        }
    }

    fn flush(&mut self, person: u64, day: i64, sessions: i64, events: i64, revenue: i64) {
        let mut top = NO_CATEGORY;
        let mut best = -1i64;
        for &(category, sum) in &self.categories {
            if sum > best || (sum == best && category < top) {
                best = sum;
                top = category;
            }
        }
        self.user.push(person);
        self.day.push(day as i32);
        self.sessions.push(sessions);
        self.events.push(events);
        self.revenue.push(revenue);
        self.top.push(top);
    }

    /// One person's events (sorted by ts) and valid clicks (sorted by ts).
    #[inline(never)]
    fn person(&mut self, person: u64, evs: &[PersonEvent], cls: &[PersonClick]) {
        let days = self.shape.days;
        let first_day = self.shape.first_day;
        let base = self.shape.base;
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut current_day = i64::MIN;
        let (mut day_sessions, mut day_events, mut day_revenue) = (0i64, 0i64, 0i64);
        self.categories.clear();
        let mut k = 0;
        while k < evs.len() {
            let start = ts_of(evs[k].packed);
            let mut end = k + 1;
            while end < evs.len() && ts_of(evs[end].packed) - ts_of(evs[end - 1].packed) <= GAP {
                end += 1;
            }
            let day = (base + start).div_euclid(DAY);
            if day != current_day {
                if current_day != i64::MIN {
                    self.flush(person, current_day, day_sessions, day_events, day_revenue);
                }
                current_day = day;
                day_sessions = 0;
                day_events = 0;
                day_revenue = 0;
                self.categories.clear();
            }
            let mut revenue = 0i64;
            let mut purchased = false;
            for e in &evs[k..end] {
                if e.packed & PURCHASE != 0 {
                    let cents = price_of(e.packed) as i64;
                    let category = category_of(e.packed);
                    revenue += cents;
                    purchased = true;
                    match self.categories.iter_mut().find(|c| c.0 == category) {
                        Some(c) => c.1 += cents,
                        None => self.categories.push((category, cents)),
                    }
                }
            }
            day_sessions += 1;
            day_events += (end - k) as i64;
            day_revenue += revenue;

            while hi < cls.len() && cls[hi].ts as i64 <= start {
                self.enter(cls[hi].campaign);
                hi += 1;
            }
            let low = start - WEEK;
            while lo < hi && (cls[lo].ts as i64) < low {
                self.leave(cls[lo].campaign);
                lo += 1;
            }
            let di = (day - first_day) as usize;
            let credited = if lo < hi {
                let last = cls[hi - 1].ts;
                let mut best = hi - 1;
                let mut b = hi - 1;
                while b > lo && cls[b - 1].ts == last {
                    b -= 1;
                    if key::cmp(cls[b].click_id, cls[best].click_id) == Ordering::Less {
                        best = b;
                    }
                }
                for &c in &self.active {
                    self.stats[(c as usize + 1) * days + di][3] += 1;
                }
                cls[best].campaign as usize + 1
            } else {
                0
            };
            let s = &mut self.stats[credited * days + di];
            s[0] += 1;
            s[1] += purchased as i64;
            s[2] += revenue;
            k = end;
        }
        if current_day != i64::MIN {
            self.flush(person, current_day, day_sessions, day_events, day_revenue);
        }
        for c in &cls[lo..hi] {
            self.leave(c.campaign);
        }
    }
}

/// Stable LSD radix sort of `v` by the low `bits` bits of `key`, 11 bits a pass
/// (all histograms counted in one read); passes whose digit never varies are
/// skipped. `tmp` is scratch.
fn radix_sort<T: Copy + Default>(v: &mut Vec<T>, tmp: &mut Vec<T>, bits: u32, key: impl Fn(&T) -> u64) {
    const DIGIT: u32 = 11;
    const BUCKETS: usize = 1 << DIGIT;
    let n = v.len();
    if n < 2 {
        return;
    }
    if n <= 64 {
        v.sort_by_key(|x| key(x)); // stable, as the passes are
        return;
    }
    let passes = bits.div_ceil(DIGIT) as usize;
    let mut counts = vec![[0u32; BUCKETS]; passes];
    for x in v.iter() {
        let k = key(x);
        for (p, count) in counts.iter_mut().enumerate() {
            count[((k >> (p as u32 * DIGIT)) as usize) & (BUCKETS - 1)] += 1;
        }
    }
    tmp.clear();
    tmp.resize(n, T::default());
    for (p, count) in counts.iter_mut().enumerate() {
        if count.iter().any(|&c| c as usize == n) {
            continue; // every key has the same digit here
        }
        let mut at = 0u32;
        for c in count.iter_mut() {
            let here = *c;
            *c = at;
            at += here;
        }
        let shift = p as u32 * DIGIT;
        for x in v.iter() {
            let d = ((key(x) >> shift) as usize) & (BUCKETS - 1);
            // SAFETY: the prefix sums keep every position below n.
            unsafe { *tmp.get_unchecked_mut(count[d] as usize) = *x };
            count[d] += 1;
        }
        std::mem::swap(v, tmp);
    }
}

/// Person -> dense rank for one partition: open addressing over (person, rank +
/// generation) slots, never cleared (a slot of an older generation is empty).
#[derive(Default)]
pub struct Ranks {
    slots: Vec<(u64, u32, u32)>,
    generation: u32,
    len: usize,
}

impl Ranks {
    fn reset(&mut self) {
        if self.slots.is_empty() {
            self.slots = vec![(0, 0, 0); 4096];
        }
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.slots.iter_mut().for_each(|s| *s = (0, 0, 0));
            self.generation = 1;
        }
        self.len = 0;
    }

    #[inline]
    fn find(&self, person: u64) -> Option<u32> {
        let mask = self.slots.len() - 1;
        let mut i = mix(person) as usize & mask;
        loop {
            let (k, rank, generation) = self.slots[i];
            if generation != self.generation {
                return None;
            }
            if k == person {
                return Some(rank);
            }
            i = (i + 1) & mask;
        }
    }

    /// The person's rank, a new one (pushed on `persons`) at first sight.
    #[inline]
    fn rank(&mut self, person: u64, persons: &mut Vec<u64>) -> u32 {
        let mask = self.slots.len() - 1;
        let mut i = mix(person) as usize & mask;
        loop {
            let (k, rank, generation) = self.slots[i];
            if generation != self.generation {
                let rank = persons.len() as u32;
                self.slots[i] = (person, rank, self.generation);
                persons.push(person);
                self.len += 1;
                if self.len * 2 > self.slots.len() {
                    self.grow(persons);
                }
                return rank;
            }
            if k == person {
                return rank;
            }
            i = (i + 1) & mask;
        }
    }

    fn grow(&mut self, persons: &[u64]) {
        let size = self.slots.len() * 2;
        self.slots = vec![(0, 0, 0); size];
        self.generation = 1;
        let mask = size - 1;
        for (rank, &person) in persons.iter().enumerate() {
            let mut i = mix(person) as usize & mask;
            while self.slots[i].2 == self.generation {
                i = (i + 1) & mask;
            }
            self.slots[i] = (person, rank as u32, self.generation);
        }
    }
}
