/// Minimal fixed-size bitset over place ids, used to hand the spatial
/// candidate set to the text search.
#[derive(Debug, Clone)]
pub struct BitSet {
    words: Vec<u64>,
    len: usize,
}

impl BitSet {
    pub fn new(capacity: usize) -> Self {
        Self {
            words: vec![0; capacity.div_ceil(64)],
            len: 0,
        }
    }

    pub fn insert(&mut self, id: u32) {
        let (w, b) = (id as usize / 64, id as usize % 64);
        if let Some(word) = self.words.get_mut(w)
            && *word & (1 << b) == 0
        {
            *word |= 1 << b;
            self.len += 1;
        }
    }

    pub fn contains(&self, id: u32) -> bool {
        let (w, b) = (id as usize / 64, id as usize % 64);
        self.words.get(w).is_some_and(|word| word & (1 << b) != 0)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let mut s = BitSet::new(130);
        s.insert(0);
        s.insert(129);
        s.insert(129);
        assert!(s.contains(0) && s.contains(129) && !s.contains(64) && !s.contains(1000));
        assert_eq!(s.len(), 2);
    }
}
