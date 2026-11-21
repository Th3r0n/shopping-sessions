//! Linked IDs: union-find over every link, each person named by the byte-wise
//! smallest of their IDs.

use std::cmp::Ordering;

use crate::hash::U64Map;
use crate::key;

pub struct Names {
    index: U64Map,
    name: Vec<u64>,
}

impl Names {
    /// The person's name for a user key: itself if it appears in no link.
    #[inline]
    pub fn get(&self, user: u64) -> u64 {
        match self.index.get(user) {
            Some(i) => self.name[i as usize],
            None => user,
        }
    }
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let grand = parent[parent[x as usize] as usize];
        parent[x as usize] = grand;
        x = grand;
    }
    x
}

pub fn build(pairs: &[Vec<(u64, u64)>]) -> Names {
    let total: usize = pairs.iter().map(Vec::len).sum();
    let mut index = U64Map::with_capacity(total + 16);
    let mut keys: Vec<u64> = Vec::with_capacity(total + 16);
    let mut parent: Vec<u32> = Vec::with_capacity(total + 16);
    let mut size: Vec<u32> = Vec::with_capacity(total + 16);
    let mut node = |k: u64, keys: &mut Vec<u64>, parent: &mut Vec<u32>, size: &mut Vec<u32>| -> u32 {
        let (i, inserted) = index.get_or_insert(k, keys.len() as u32);
        if inserted {
            keys.push(k);
            parent.push(i);
            size.push(1);
        }
        i
    };
    for list in pairs {
        for &(a, b) in list {
            let ia = node(a, &mut keys, &mut parent, &mut size);
            let ib = node(b, &mut keys, &mut parent, &mut size);
            let ra = find(&mut parent, ia);
            let rb = find(&mut parent, ib);
            if ra != rb {
                let (big, small) = if size[ra as usize] >= size[rb as usize] { (ra, rb) } else { (rb, ra) };
                parent[small as usize] = big;
                size[big as usize] += size[small as usize];
            }
        }
    }
    let n = keys.len();
    let mut best = vec![0u64; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        if best[r] == 0 || key::cmp(keys[i], best[r]) == Ordering::Less {
            best[r] = keys[i];
        }
    }
    let name = (0..n).map(|i| best[find(&mut parent, i as u32) as usize]).collect();
    Names { index, name }
}
