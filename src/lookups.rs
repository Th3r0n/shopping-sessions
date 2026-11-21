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
    /// Every ID inserted, until `seal`.
    ids: Vec<Vec<u8>>,
    /// When every ID is `prefix` + a canonical decimal number (like `c_123`), the
    /// values indexed by that number (value + 1; 0 = absent): any other string
    /// cannot be one of the IDs.
    direct: Option<(Vec<u8>, Vec<u32>)>,
}

impl IdMap {
    fn new() -> Self {
        Self {
            slots: vec![[0, 0]; 16],
            mask: 15,
            len: 0,
            other: std::collections::HashMap::new(),
            ids: Vec::new(),
            direct: None,
        }
    }

    /// Builds the direct index when every ID has the `prefix` + number shape.
    fn seal(&mut self) {
        let ids = std::mem::take(&mut self.ids);
        let Some(first) = ids.first() else { return };
        let prefix: Vec<u8> = first.iter().take_while(|b| !b.is_ascii_digit()).copied().collect();
        let number = |id: &[u8]| -> Option<usize> { canonical(id.strip_prefix(prefix.as_slice())?) };
        if prefix.is_empty() || !ids.iter().all(|id| number(id).is_some()) {
            return;
        }
        let max = ids.iter().filter_map(|id| number(id)).max().unwrap_or(0);
        if max > 4 * ids.len() + 1024 {
            return; // too sparse for a table
        }
        let mut table = vec![0u32; max + 1];
        for id in &ids {
            table[number(id).unwrap()] = self.get_hashed(id).unwrap() + 1;
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
        self.ids.push(id.to_vec());
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
        let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        small_rows::<2>(&dir.join("products.csv"), |[product, category]| {
            if !product.is_empty() && !category.is_empty() {
                rows.push((product.to_vec(), category.to_vec()));
            }
        });
        let mut categories: Vec<Vec<u8>> = rows.iter().map(|(_, c)| c.clone()).collect();
        categories.push(b"unknown".to_vec());
        categories.sort_unstable();
        categories.dedup();
        assert!(categories.len() <= u16::MAX as usize, "too many categories");
        let id_of = |name: &[u8]| categories.binary_search_by(|c| c.as_slice().cmp(name)).unwrap() as u16;
        let unknown = id_of(b"unknown");
        let mut map = IdMap::new();
        for (product, category) in &rows {
            map.insert(product, id_of(category) as u32);
        }
        map.seal();
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
