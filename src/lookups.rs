//! manifest.json, products.csv and campaigns.csv.

use std::path::Path;

use crate::csv;
use crate::hash::mix;
use crate::key;
use crate::ts;

pub struct Window {
    pub start: i64,
    pub end: i64,
}

pub fn read_manifest(dir: &Path) -> Window {
    let path = dir.join("manifest.json");
    let text = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let json: serde_json::Value = serde_json::from_slice(&text).expect("manifest.json is not JSON");
    let field = |name: &str| {
        let value = json[name].as_str().unwrap_or_else(|| panic!("manifest.json has no {name}"));
        ts::parse(value.as_bytes()).unwrap_or_else(|| panic!("manifest.json {name} is not a timestamp: {value}"))
    };
    Window {
        start: field("window_start"),
        end: field("window_end"),
    }
}

/// The well-formed data rows of a small CSV file.
fn small_rows<const N: usize>(path: &Path, mut f: impl FnMut([&[u8]; N])) {
    let Ok(data) = std::fs::read(path) else {
        return;
    };
    let Some(first) = csv::data_start(&data) else {
        return;
    };
    csv::for_each_line(&data, first, first, data.len(), |line| {
        let mut out = [&b""[..]; N];
        if csv::fields::<N>(line, &mut out) {
            f(out);
        }
    });
}

/// An exact map from ID bytes to a small index: packable IDs in a small
/// open-addressing table of (key, value + 1) slots, the rest in a HashMap.
pub struct IdMap {
    slots: Vec<[u64; 2]>,
    mask: usize,
    len: usize,
    other: std::collections::HashMap<Vec<u8>, u32>,
    /// While inserting: the common `prefix` + number shape, if every ID so far has
    /// it, and the (number, value) pairs in insertion order.
    shape: Option<(Vec<u8>, Vec<(usize, u32)>)>,
    started: bool,
    /// When every ID is `prefix` + a canonical decimal number (like `c_123`), the
    /// values indexed by that number (value + 1; 0 = absent): any other string
    /// cannot be one of the IDs.
    direct: Option<(Vec<u8>, Vec<u32>)>,
}

impl IdMap {
    fn new() -> Self {
        Self::with_capacity(8)
    }

    /// Room for about `n` IDs without growing.
    fn with_capacity(n: usize) -> Self {
        let size = (n.max(8) * 2).next_power_of_two();
        Self {
            slots: vec![[0, 0]; size],
            mask: size - 1,
            len: 0,
            other: std::collections::HashMap::new(),
            shape: None,
            started: false,
            direct: None,
        }
    }

    /// Records `id` for the direct index while it keeps the `prefix` + number shape.
    fn note_shape(&mut self, id: &[u8], value: u32) {
        if !self.started {
            self.started = true;
            let prefix: Vec<u8> = id.iter().take_while(|b| !b.is_ascii_digit()).copied().collect();
            if !prefix.is_empty() {
                self.shape = Some((prefix, Vec::new()));
            }
        }
        if let Some((prefix, pairs)) = &mut self.shape {
            if pairs.capacity() == 0 {
                pairs.reserve(self.slots.len() / 2);
            }
            match id.strip_prefix(prefix.as_slice()).and_then(canonical) {
                Some(n) => pairs.push((n, value)),
                None => self.shape = None,
            }
        }
    }

    /// Builds the direct index when every ID had the `prefix` + number shape.
    fn seal(&mut self) {
        let Some((prefix, pairs)) = self.shape.take() else { return };
        let max = pairs.iter().map(|&(n, _)| n).max().unwrap_or(0);
        if pairs.is_empty() || max > 4 * pairs.len() + 1024 {
            return; // nothing to index, or too sparse for a table
        }
        let mut table = vec![0u32; max + 1];
        for (n, value) in pairs {
            table[n] = value + 1; // later inserts overwrite, as in the hashed map
        }
        self.direct = Some((prefix, table));
    }

    fn insert_packed(&mut self, k: u64, value: u32) {
        if (self.len + 1) * 2 > self.slots.len() {
            let bigger = vec![[0, 0]; self.slots.len() * 2];
            let old = std::mem::replace(&mut self.slots, bigger);
            self.mask = self.slots.len() - 1;
            self.len = 0;
            for [key, v] in old {
                if key != 0 {
                    self.insert_packed(key, (v - 1) as u32);
                }
            }
        }
        let mut i = mix(k) as usize & self.mask;
        loop {
            if self.slots[i][0] == 0 {
                self.slots[i] = [k, value as u64 + 1];
                self.len += 1;
                return;
            }
            if self.slots[i][0] == k {
                self.slots[i][1] = value as u64 + 1;
                return;
            }
            i = (i + 1) & self.mask;
        }
    }

    fn insert(&mut self, id: &[u8], value: u32) {
        self.note_shape(id, value);
        match key::pack(id) {
            Some(k) => self.insert_packed(k, value),
            None => {
                self.other.insert(id.to_vec(), value);
            }
        }
    }

    #[inline]
    pub fn get(&self, id: &[u8]) -> Option<u32> {
        if let Some((prefix, table)) = &self.direct {
            let n = canonical(id.strip_prefix(prefix.as_slice())?)?;
            return match table.get(n) {
                Some(&v) if v != 0 => Some(v - 1),
                _ => None,
            };
        }
        self.get_hashed(id)
    }

    /// Replaces every stored value v by `to[v]` (before `seal`).
    fn remap(&mut self, to: &[u32]) {
        for slot in self.slots.iter_mut() {
            if slot[0] != 0 {
                slot[1] = to[(slot[1] - 1) as usize] as u64 + 1;
            }
        }
        for v in self.other.values_mut() {
            *v = to[*v as usize];
        }
        if let Some((_, pairs)) = &mut self.shape {
            for (_, v) in pairs.iter_mut() {
                *v = to[*v as usize];
            }
        }
    }

    #[inline]
    fn get_hashed(&self, id: &[u8]) -> Option<u32> {
        match key::pack(id) {
            Some(k) => {
                let mut i = mix(k) as usize & self.mask;
                loop {
                    let [key, v] = self.slots[i];
                    if key == k {
                        return Some((v - 1) as u32);
                    }
                    if key == 0 {
                        return None;
                    }
                    i = (i + 1) & self.mask;
                }
            }
            None => self.other.get(id).copied(),
        }
    }
}

/// A canonical decimal number (ASCII digits, no leading zero, at most 9 digits).
#[inline]
fn canonical(digits: &[u8]) -> Option<usize> {
    if digits.is_empty() || digits.len() > 9 || (digits[0] == b'0' && digits.len() > 1) {
        return None;
    }
    let mut n = 0usize;
    for &c in digits {
        let d = c.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        n = n * 10 + d as usize;
    }
    Some(n)
}

pub struct Products {
    map: IdMap,
    /// Category names in byte-wise order; an id is an index here.
    pub categories: Vec<Vec<u8>>,
    pub unknown: u16,
}

impl Products {
    pub fn read(dir: &Path) -> Self {
        // Products are mapped to provisional category numbers (first seen order),
        // renumbered at the end in byte-wise name order.
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut index: std::collections::HashMap<Vec<u8>, u32> = std::collections::HashMap::new();
        let path = dir.join("products.csv");
        let bytes = std::fs::metadata(&path).map(|m| m.len() as usize).unwrap_or(0);
        let mut map = IdMap::with_capacity(bytes / 10);
        small_rows::<2>(&path, |[product, category]| {
            if !product.is_empty() && !category.is_empty() {
                let provisional = match names.iter().position(|n| n.as_slice() == category) {
                    Some(i) => i as u32,
                    None if names.len() < 64 => {
                        names.push(category.to_vec());
                        names.len() as u32 - 1
                    }
                    None => {
                        let next = names.len() as u32 + index.len() as u32;
                        *index.entry(category.to_vec()).or_insert(next)
                    }
                };
                map.insert(product, provisional);
            }
        });
        // every distinct name, in provisional order
        let mut all: Vec<(Vec<u8>, u32)> = names.into_iter().enumerate().map(|(i, n)| (n, i as u32)).collect();
        all.extend(index);
        let mut categories: Vec<Vec<u8>> = all.iter().map(|(n, _)| n.clone()).collect();
        categories.push(b"unknown".to_vec());
        categories.sort_unstable();
        categories.dedup();
        assert!(categories.len() <= u16::MAX as usize, "too many categories");
        let id_of = |name: &[u8]| categories.binary_search_by(|c| c.as_slice().cmp(name)).unwrap() as u32;
        let mut renumber = vec![0u32; all.len()];
        for (name, provisional) in &all {
            renumber[*provisional as usize] = id_of(name);
        }
        map.remap(&renumber);
        map.seal();
        let unknown = id_of(b"unknown") as u16;
        Self { map, categories, unknown }
    }

    #[inline]
    pub fn category(&self, product: &[u8]) -> u16 {
        match self.map.get(product) {
            Some(c) => c as u16,
            None => self.unknown,
        }
    }
}

pub struct Campaigns {
    map: IdMap,
    pub ids: Vec<Vec<u8>>,
    pub channels: Vec<Vec<u8>>,
}

impl Campaigns {
    pub fn read(dir: &Path) -> Self {
        let mut map = IdMap::new();
        let mut ids = Vec::new();
        let mut channels = Vec::new();
        small_rows::<2>(&dir.join("campaigns.csv"), |[campaign, channel]| {
            if !campaign.is_empty() && map.get(campaign).is_none() {
                map.insert(campaign, ids.len() as u32);
                ids.push(campaign.to_vec());
                channels.push(channel.to_vec());
            }
        });
        map.seal();
        Self { map, ids, channels }
    }

    #[inline]
    pub fn get(&self, campaign: &[u8]) -> Option<u32> {
        self.map.get(campaign)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_index_is_exact() {
        let mut m = IdMap::new();
        for i in 1..=2000u32 {
            m.insert(format!("c_{i}").as_bytes(), i - 1);
        }
        m.seal();
        assert!(m.direct.is_some());
        for (id, want) in [("c_1", Some(0)), ("c_2000", Some(1999)), ("c_2001", None), ("c_0", None), ("c_012", None),
                           ("C_12", None), ("c_", None), ("c_1 ", None), ("c_12x", None), ("x_12", None), ("", None),
                           ("c_99999999999", None)] {
            assert_eq!(m.get(id.as_bytes()), want, "{id}");
        }
        // a map whose IDs are not all prefix + number keeps the hashed lookup
        let mut m = IdMap::new();
        m.insert(b"c_1", 0);
        m.insert(b"c_01", 1);
        m.seal();
        assert!(m.direct.is_none());
        assert_eq!(m.get(b"c_01"), Some(1));
        assert_eq!(m.get(b"c_1"), Some(0));
    }
}
