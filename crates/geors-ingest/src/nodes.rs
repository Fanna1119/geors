//! Disk-backed node coordinate store.
//!
//! Only nodes referenced by interesting ways are kept. Their ids are
//! sorted and deduplicated with an external merge sort (sequential disk
//! access even when the ids far exceed RAM) into a scratch file, next to a
//! parallel coordinate array (two `i32`s per node). Both live in the page cache, not on the heap, so the OS can
//! evict them under memory pressure instead of the import being killed.
//! Cost: 16 bytes per needed node.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use geors_core::LonLat;
use geors_core::geom::{from_e7, to_e7};
use memmap2::MmapMut;
use tracing::info;

use crate::extsort::{ExternalSorter, buffer_bytes};

const MISSING: i32 = i32::MIN;

/// Collects node ids (duplicates allowed); sorted and deduplicated by
/// [`NodeCoords::build`].
pub struct IdSink {
    sorter: ExternalSorter<i64>,
    path: PathBuf,
}

impl IdSink {
    /// `path` is where the final sorted id file goes; sort runs are written
    /// next to it.
    pub fn create(path: &Path) -> io::Result<Self> {
        let dir = path.parent().unwrap_or(Path::new("."));
        let prefix = path.file_name().unwrap_or_default().to_string_lossy();
        Ok(Self {
            sorter: ExternalSorter::new(dir, &prefix, buffer_bytes(), true),
            path: path.to_path_buf(),
        })
    }

    pub fn push(&mut self, id: i64) -> io::Result<()> {
        self.sorter.push(id)
    }

    pub fn extend(&mut self, ids: impl IntoIterator<Item = i64>) -> io::Result<()> {
        self.sorter.extend(ids)
    }
}

fn map(path: &Path, len: u64) -> io::Result<Option<MmapMut>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.set_len(len)?;
    if len == 0 {
        return Ok(None);
    }
    // SAFETY: scratch files private to this import; nothing else maps them.
    Ok(Some(unsafe { MmapMut::map_mut(&file)? }))
}

/// Sorted, deduplicated ids of the needed nodes (read-only once built).
pub struct NodeIds {
    map: Option<MmapMut>,
    len: usize,
    /// First id of every block of [`BLOCK`] ids, in RAM (8 bytes per 4 kB
    /// block: 4.6 MB for Germany's 147 M needed nodes).
    fence: Vec<i64>,
}

/// Ids per block: 512 * 8 bytes = one 4 kB page.
const BLOCK: usize = 512;

impl NodeIds {
    fn new(map: Option<MmapMut>, len: usize) -> Self {
        let mut ids = Self {
            map,
            len,
            fence: Vec::new(),
        };
        ids.fence = ids.slice().iter().step_by(BLOCK).copied().collect();
        ids
    }

    fn slice(&self) -> &[i64] {
        match &self.map {
            Some(m) => &bytemuck::cast_slice::<u8, i64>(m)[..self.len],
            None => &[],
        }
    }

    /// Position of `id` in the coordinate array, if the node is needed.
    ///
    /// The fence narrows the search to one block, so a lookup touches a
    /// single page of the id file instead of ~25 pages spread over it.
    /// That keeps imports fast when the page cache is much smaller than
    /// the store (containers with little memory).
    pub fn index(&self, id: i64) -> Option<usize> {
        let block = self.fence.partition_point(|&f| f <= id).checked_sub(1)?;
        let start = block * BLOCK;
        let end = (start + BLOCK).min(self.len);
        self.slice()[start..end]
            .binary_search(&id)
            .ok()
            .map(|i| start + i)
    }
}

/// Coordinates, parallel to [`NodeIds`].
pub struct CoordArray {
    map: Option<MmapMut>,
}

impl CoordArray {
    fn values(&self) -> &[i32] {
        self.map
            .as_deref()
            .map(bytemuck::cast_slice)
            .unwrap_or_default()
    }

    pub fn set(&mut self, i: usize, lon_e7: i32, lat_e7: i32) {
        if let Some(c) = self.map.as_deref_mut() {
            let c: &mut [i32] = bytemuck::cast_slice_mut(c);
            c[2 * i] = lon_e7;
            c[2 * i + 1] = lat_e7;
        }
    }

    fn get(&self, i: usize) -> Option<LonLat> {
        let c = self.values();
        (c[2 * i] != MISSING).then(|| LonLat::new(from_e7(c[2 * i]), from_e7(c[2 * i + 1])))
    }
}

pub struct NodeCoords {
    ids: NodeIds,
    coords: CoordArray,
    /// Scratch files, deleted on drop (they can be gigabytes).
    files: [PathBuf; 2],
}

impl Drop for NodeCoords {
    fn drop(&mut self) {
        self.ids.map = None;
        self.coords.map = None;
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

impl NodeCoords {
    /// Sort and deduplicate the collected ids; allocate the coordinate array
    /// at `coords_path`.
    pub fn build(sink: IdSink, coords_path: &Path) -> io::Result<Self> {
        let t = Instant::now();
        let IdSink { sorter, path } = sink;
        let pushed = sorter.pushed();
        let mut sorted = sorter.finish()?;
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&path)?);
        let mut len = 0usize;
        while let Some(id) = sorted.next_record()? {
            out.write_all(&id.to_ne_bytes())?;
            len += 1;
        }
        out.flush()?;
        drop(out);
        drop(sorted);
        info!(references = pushed, needed_nodes = len, elapsed = ?t.elapsed(), "ids sorted");
        let ids = map(&path, len as u64 * 8)?;
        let mut coords = map(coords_path, len as u64 * 8)?;
        if let Some(c) = coords.as_deref_mut() {
            bytemuck::cast_slice_mut::<u8, i32>(c).fill(MISSING);
        }
        Ok(Self {
            ids: NodeIds::new(ids, len),
            coords: CoordArray { map: coords },
            files: [path, coords_path.to_path_buf()],
        })
    }

    /// Separate read-only id lookup from coordinate writes, so lookups can
    /// run on many threads while one thread stores the results.
    pub fn parts_mut(&mut self) -> (&NodeIds, &mut CoordArray) {
        (&self.ids, &mut self.coords)
    }

    pub fn len(&self) -> usize {
        self.ids.len
    }

    pub fn is_empty(&self) -> bool {
        self.ids.len == 0
    }

    pub fn set(&mut self, id: i64, p: LonLat) {
        if let Some(i) = self.ids.index(id) {
            self.coords.set(i, to_e7(p.lon), to_e7(p.lat));
        }
    }

    pub fn get(&self, id: i64) -> Option<LonLat> {
        self.coords.get(self.ids.index(id)?)
    }

    /// Resolve a node list, skipping nodes missing from the extract.
    pub fn line(&self, refs: &[i64]) -> Vec<LonLat> {
        refs.iter().filter_map(|&id| self.get(id)).collect()
    }

    pub fn missing(&self) -> usize {
        self.coords
            .values()
            .as_chunks::<2>()
            .0
            .iter()
            .filter(|p| p[0] == MISSING)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = IdSink::create(&dir.path().join("ids")).unwrap();
        sink.extend([5, 3, 9, 3, 5, 1]).unwrap();
        let mut nodes = NodeCoords::build(sink, &dir.path().join("coords")).unwrap();
        assert_eq!(nodes.len(), 4);
        nodes.set(3, LonLat::new(9.5, 47.1));
        nodes.set(9, LonLat::new(-0.1, 51.5));
        nodes.set(42, LonLat::new(1.0, 1.0)); // not needed: ignored
        assert_eq!(nodes.get(3), Some(LonLat::new(9.5, 47.1)));
        assert_eq!(nodes.get(5), None);
        assert_eq!(nodes.get(42), None);
        assert_eq!(nodes.line(&[3, 5, 9]).len(), 2);
        assert_eq!(nodes.missing(), 2);
        drop(nodes);
        assert!(!dir.path().join("ids").exists() && !dir.path().join("coords").exists());
    }

    #[test]
    fn lookups_across_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = IdSink::create(&dir.path().join("ids")).unwrap();
        // 5000 ids with gaps: spans several fence blocks.
        sink.extend((0..5000).map(|i| i * 3 + 7)).unwrap();
        let mut nodes = NodeCoords::build(sink, &dir.path().join("coords")).unwrap();
        for i in (0..5000).step_by(97) {
            nodes.set(i * 3 + 7, LonLat::new(i as f64 / 1e4, 1.0));
        }
        for i in 0..5000 {
            let got = nodes.get(i * 3 + 7);
            assert_eq!(got.is_some(), i % 97 == 0, "{i}");
            assert_eq!(nodes.get(i * 3 + 8), None);
        }
        assert_eq!(nodes.get(6), None);
        assert_eq!(nodes.get(i64::MAX), None);
    }

    #[test]
    fn empty() {
        let dir = tempfile::tempdir().unwrap();
        let sink = IdSink::create(&dir.path().join("ids")).unwrap();
        let nodes = NodeCoords::build(sink, &dir.path().join("coords")).unwrap();
        assert!(nodes.is_empty());
        assert_eq!(nodes.get(1), None);
    }
}
