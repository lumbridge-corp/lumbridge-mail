//! A set of UIDs kept as sorted, merged ranges, written the way IMAP writes sequence sets (`1:5,9,12:20`).
//! The cursor stores which UIDs of a folder we have reported, so it has to stay small for big folders.

use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UidSet {
    /// Sorted, non-overlapping, non-adjacent inclusive ranges.
    ranges: Vec<(u32, u32)>,
}

impl UidSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_uids(uids: impl IntoIterator<Item = u32>) -> Self {
        let sorted: BTreeSet<u32> = uids.into_iter().filter(|u| *u > 0).collect();
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for u in sorted {
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == u => *end = u,
                _ => ranges.push((u, u)),
            }
        }
        Self { ranges }
    }

    /// Parses `1:5,9` (also `5:1`). Garbage pieces are skipped; `*` is not allowed here.
    pub fn parse(s: &str) -> Self {
        let mut out = Self::new();
        for piece in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (a, b) = match piece.split_once(':') {
                Some((a, b)) => (a.parse::<u32>(), b.parse::<u32>()),
                None => (piece.parse::<u32>(), piece.parse::<u32>()),
            };
            if let (Ok(a), Ok(b)) = (a, b)
                && a > 0
                && b > 0
            {
                out.insert_range(a.min(b), a.max(b));
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ranges.iter().map(|(a, b)| (b - a) as usize + 1).sum()
    }

    pub fn contains(&self, uid: u32) -> bool {
        self.ranges
            .binary_search_by(|(a, b)| {
                if uid < *a {
                    std::cmp::Ordering::Greater
                } else if uid > *b {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    #[cfg(test)]
    pub fn max(&self) -> Option<u32> {
        self.ranges.last().map(|r| r.1)
    }

    #[cfg(test)]
    pub fn min(&self) -> Option<u32> {
        self.ranges.first().map(|r| r.0)
    }

    pub fn insert(&mut self, uid: u32) {
        if uid > 0 {
            self.insert_range(uid, uid);
        }
    }

    pub fn insert_range(&mut self, a: u32, b: u32) {
        let (mut a, mut b) = (a.min(b), a.max(b));
        let mut out = Vec::with_capacity(self.ranges.len() + 1);
        let mut placed = false;
        for &(x, y) in &self.ranges {
            if y.saturating_add(1) < a {
                out.push((x, y));
            } else if b.saturating_add(1) < x {
                if !placed {
                    out.push((a, b));
                    placed = true;
                }
                out.push((x, y));
            } else {
                a = a.min(x);
                b = b.max(y);
            }
        }
        if !placed {
            out.push((a, b));
        }
        self.ranges = out;
    }

    pub fn extend(&mut self, uids: impl IntoIterator<Item = u32>) {
        for u in uids {
            self.insert(u);
        }
    }

    pub fn remove(&mut self, uid: u32) {
        let mut out = Vec::with_capacity(self.ranges.len() + 1);
        for &(a, b) in &self.ranges {
            if uid < a || uid > b {
                out.push((a, b));
                continue;
            }
            if a < uid {
                out.push((a, uid - 1));
            }
            if uid < b {
                out.push((uid + 1, b));
            }
        }
        self.ranges = out;
    }

    /// Every UID, ascending. Only for sets known to be small enough to list.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        self.ranges.iter().flat_map(|&(a, b)| a..=b)
    }

    /// Members not in `other`.
    pub fn difference(&self, other: &UidSet) -> Vec<u32> {
        self.iter().filter(|u| !other.contains(*u)).collect()
    }

    /// The `n` highest UIDs, ascending.
    pub fn newest(&self, n: usize) -> Vec<u32> {
        let mut v: Vec<u32> = self.iter().rev().take(n).collect();
        v.reverse();
        v
    }

    /// The IMAP sequence-set form; empty string for an empty set.
    pub fn to_imap(&self) -> String {
        self.ranges
            .iter()
            .map(|&(a, b)| if a == b { a.to_string() } else { format!("{a}:{b}") })
            .collect::<Vec<_>>()
            .join(",")
    }
}

impl std::fmt::Display for UidSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_imap())
    }
}

/// Splits UIDs into IMAP sets of at most `chunk` members each, so command lines stay short.
pub fn chunks(uids: &[u32], chunk: usize) -> Vec<UidSet> {
    uids.chunks(chunk.max(1)).map(|c| UidSet::from_uids(c.iter().copied())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_merge_and_split() {
        let mut s = UidSet::from_uids([5, 1, 2, 3, 9, 10, 4]);
        assert_eq!(s.to_imap(), "1:5,9:10");
        assert_eq!(s.len(), 7);
        s.insert(7);
        assert_eq!(s.to_imap(), "1:5,7,9:10");
        s.insert(8);
        s.insert(6);
        assert_eq!(s.to_imap(), "1:10");
        s.remove(4);
        assert_eq!(s.to_imap(), "1:3,5:10");
        s.remove(1);
        s.remove(10);
        assert_eq!(s.to_imap(), "2:3,5:9");
        assert!(s.contains(6) && !s.contains(4) && !s.contains(1));
        assert_eq!((s.min(), s.max()), (Some(2), Some(9)));
        assert_eq!(s.newest(3), vec![7, 8, 9]);
    }

    #[test]
    fn parse_round_trips_and_ignores_garbage() {
        let s = UidSet::parse("10:12, 3,x,0,5:4,7");
        assert_eq!(s.to_imap(), "3:5,7,10:12");
        assert_eq!(UidSet::parse(&s.to_imap()), s);
        assert!(UidSet::parse("").is_empty());
        let mut big = UidSet::new();
        big.insert_range(1, 100_000);
        assert_eq!(big.to_imap(), "1:100000");
        assert_eq!(big.len(), 100_000);
        let other = UidSet::from_uids([1, 2, 3]);
        assert_eq!(UidSet::from_uids([1, 2, 3, 4, 6]).difference(&other), vec![4, 6]);
    }

    #[test]
    fn chunks_are_bounded() {
        let uids: Vec<u32> = (1..=10).collect();
        let c = chunks(&uids, 4);
        assert_eq!(c.iter().map(|s| s.to_imap()).collect::<Vec<_>>(), vec!["1:4", "5:8", "9:10"]);
    }
}
