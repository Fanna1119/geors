/// Set of place ids passing the hard spatial filter, handed to the text
/// search. Sparse sets (the usual case: a radius in a large partition) are
/// a sorted id list; dense ones a bitmap. Either way at most one bit per
/// place, often far less: a 3 km radius in Germany is a few kB instead of a
/// 3 MB bitmap per query.
#[derive(Debug, Clone)]
pub enum BitSet {
    Sparse(Vec<u32>),
    Dense { words: Vec<u64>, len: usize },
}

impl BitSet {
    /// Build from ids (any order, duplicates allowed) in `0..capacity`.
    pub fn from_ids(mut ids: Vec<u32>, capacity: usize) -> Self {
        ids.sort_unstable();
        ids.dedup();
        // A sorted u32 list costs 32 bits per id, a bitmap 1 bit per place.
        if ids.len() * 32 <= capacity {
            ids.shrink_to_fit();
            return BitSet::Sparse(ids);
        }
        let mut words = vec![0u64; capacity.div_ceil(64)];
        let mut len = 0;
        for id in ids {
            if let Some(w) = words.get_mut(id as usize / 64) {
                *w |= 1 << (id % 64);
                len += 1;
            }
        }
        BitSet::Dense { words, len }
    }

    pub fn contains(&self, id: u32) -> bool {
        match self {
            BitSet::Sparse(ids) => ids.binary_search(&id).is_ok(),
            BitSet::Dense { words, .. } => words
                .get(id as usize / 64)
                .is_some_and(|w| w & (1 << (id % 64)) != 0),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            BitSet::Sparse(ids) => ids.len(),
            BitSet::Dense { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_and_dense_agree() {
        let ids = [129, 0, 129, 64, 5000];
        for capacity in [10_000, 100] {
            let s = BitSet::from_ids(
                ids.iter()
                    .copied()
                    .filter(|&i| (i as usize) < capacity)
                    .collect(),
                capacity,
            );
            assert!(s.contains(0));
            assert!(!s.contains(1));
            assert_eq!(s.contains(5000), capacity > 5000);
        }
        assert!(matches!(
            BitSet::from_ids(vec![1, 2], 10_000),
            BitSet::Sparse(_)
        ));
        assert!(matches!(
            BitSet::from_ids((0..100).collect(), 200),
            BitSet::Dense { .. }
        ));
        assert_eq!(BitSet::from_ids((0..100).collect(), 200).len(), 100);
    }
}
