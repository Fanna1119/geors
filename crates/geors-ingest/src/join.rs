//! Sorted-join node lookup, for extracts whose node coordinates do not fit
//! in RAM.
//!
//! The memory-mode import looks up every node of every way in a coordinate
//! store; once that store is much larger than RAM, each lookup is a random
//! disk read. Here the lookups are turned into sequential passes instead:
//!
//! 1. pass 2 records `(node id, way id << 16 | position)` for each node
//!    reference of a feature way, sorted externally by node id;
//! 2. pass 3 reads nodes in id order (file order of a sorted PBF) and sweeps
//!    the sorted references alongside, emitting
//!    `(way id << 16 | position, coordinate)`, sorted externally by way;
//! 3. the result is written as a way-geometry file in way-id order, which
//!    pass 4 reads with a cursor, sequentially, block by block.
//!
//! Costs extra temporary disk (16 bytes per reference, twice) and sorting
//! time, so it only pays off when the coordinates do not fit in RAM.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use geors_core::LonLat;
use geors_core::geom::from_e7;
use memmap2::Mmap;

use crate::extsort::{ExternalSorter, buffer_bytes, Sorted};

/// Ways longer than this cannot be packed (OSM's limit is 2,000 nodes).
const MAX_POSITION: usize = u16::MAX as usize;
/// Ways per fence entry of the geometry file.
const FENCE_EVERY: usize = 1024;

fn pack_way_pos(way: i64, pos: usize) -> i64 {
    (way << 16) | pos as i64
}

fn pack_coord(lon: i32, lat: i32) -> i64 {
    (((lon as u32 as u64) << 32) | lat as u32 as u64) as i64
}

fn unpack_coord(v: i64) -> (i32, i32) {
    let v = v as u64;
    ((v >> 32) as u32 as i32, v as u32 as i32)
}

/// Node references of feature ways, to be sorted by node id.
pub struct RefSink {
    sorter: ExternalSorter<[i64; 2]>,
    dir: PathBuf,
}

impl RefSink {
    pub fn new(dir: &Path) -> Self {
        Self {
            sorter: ExternalSorter::new(dir, "way-refs", buffer_bytes(), false),
            dir: dir.to_path_buf(),
        }
    }

    pub fn push_way(&mut self, way: i64, refs: &[i64]) -> io::Result<()> {
        if refs.len() > MAX_POSITION || !(0..1 << 47).contains(&way) {
            return Ok(()); // not valid OSM data; ignored like a missing way
        }
        for (pos, &node) in refs.iter().enumerate() {
            self.sorter.push([node, pack_way_pos(way, pos)])?;
        }
        Ok(())
    }

    pub fn references(&self) -> u64 {
        self.sorter.pushed()
    }

    /// Start the sweep over nodes in ascending id order.
    pub fn into_joiner(self) -> io::Result<Joiner> {
        let mut refs = self.sorter.finish()?;
        let next = refs.next_record()?;
        Ok(Joiner {
            refs,
            next,
            geoms: ExternalSorter::new(&self.dir, "way-geoms", buffer_bytes(), false),
            last_node: i64::MIN,
            matched: 0,
        })
    }
}

/// Sweeps sorted node references alongside the nodes of the extract.
pub struct Joiner {
    refs: Sorted<[i64; 2]>,
    next: Option<[i64; 2]>,
    geoms: ExternalSorter<[i64; 2]>,
    last_node: i64,
    matched: u64,
}

impl Joiner {
    /// Feed the next node; nodes must come in strictly ascending id order.
    pub fn node(&mut self, id: i64, lon: i32, lat: i32) -> io::Result<()> {
        if id <= self.last_node {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "nodes are not sorted by id; import with --node-lookup memory",
            ));
        }
        self.last_node = id;
        // References to nodes missing from the extract are skipped.
        while let Some([node, wp]) = self.next {
            if node > id {
                break;
            }
            if node == id {
                self.geoms.push([wp, pack_coord(lon, lat)])?;
                self.matched += 1;
            }
            self.next = self.refs.next_record()?;
        }
        Ok(())
    }

    /// Sort the found coordinates by way and write the geometry file.
    pub fn finish(self, path: &Path) -> io::Result<(WayGeoms, u64)> {
        drop(self.refs);
        let matched = self.matched;
        Ok((WayGeoms::build(self.geoms.finish()?, path)?, matched))
    }
}

/// Coordinates of each feature way, in way-id order.
///
/// Record layout: `way id: i64, n: u32, n * (lon_e7: i32, lat_e7: i32)`.
pub struct WayGeoms {
    map: Option<Mmap>,
    /// `(way id, byte offset)` of every [`FENCE_EVERY`]-th record.
    fence: Vec<(i64, usize)>,
    path: PathBuf,
}

/// Per-worker read position; ways of a PBF block come in ascending order.
#[derive(Default)]
pub struct WayCursor {
    offset: usize,
    /// Way id of the record at `offset`, if positioned.
    way: Option<i64>,
}

impl WayGeoms {
    fn build(mut sorted: Sorted<[i64; 2]>, path: &Path) -> io::Result<Self> {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(path)?);
        let mut fence = Vec::new();
        let (mut offset, mut ways) = (0usize, 0usize);
        let mut current: Option<i64> = None;
        let mut coords: Vec<i64> = Vec::new();
        let mut flush = |way: i64, coords: &mut Vec<i64>, out: &mut BufWriter<File>| -> io::Result<()> {
            if ways.is_multiple_of(FENCE_EVERY) {
                fence.push((way, offset));
            }
            out.write_all(&way.to_le_bytes())?;
            out.write_all(&(coords.len() as u32).to_le_bytes())?;
            for &c in coords.iter() {
                let (lon, lat) = unpack_coord(c);
                out.write_all(&lon.to_le_bytes())?;
                out.write_all(&lat.to_le_bytes())?;
            }
            offset += 12 + coords.len() * 8;
            ways += 1;
            coords.clear();
            Ok(())
        };
        while let Some([wp, coord]) = sorted.next_record()? {
            let way = wp >> 16;
            if current.is_some_and(|w| w != way) {
                flush(current.unwrap(), &mut coords, &mut out)?;
            }
            current = Some(way);
            coords.push(coord);
        }
        if let Some(way) = current {
            flush(way, &mut coords, &mut out)?;
        }
        out.flush()?;
        drop(out);
        let file = File::open(path)?;
        let map = if file.metadata()?.len() == 0 {
            None
        } else {
            // SAFETY: scratch file private to this import, written above.
            Some(unsafe { Mmap::map(&file)? })
        };
        Ok(Self {
            map,
            fence,
            path: path.to_path_buf(),
        })
    }

    fn bytes(&self) -> &[u8] {
        self.map.as_deref().unwrap_or_default()
    }

    fn header(&self, offset: usize) -> Option<(i64, usize)> {
        let b = self.bytes().get(offset..offset + 12)?;
        let way = i64::from_le_bytes(b[0..8].try_into().unwrap());
        let n = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        Some((way, n))
    }

    /// Coordinates of `way` (empty if none of its nodes were found).
    pub fn line(&self, cursor: &mut WayCursor, way: i64) -> Vec<LonLat> {
        // Continue from the cursor when moving forward, else jump via the fence.
        if cursor.way.is_none_or(|w| w > way) {
            let i = self.fence.partition_point(|(w, _)| *w <= way);
            let Some(&(w, offset)) = i.checked_sub(1).and_then(|i| self.fence.get(i)) else {
                return Vec::new();
            };
            *cursor = WayCursor {
                offset,
                way: Some(w),
            };
        }
        while let Some((w, n)) = self.header(cursor.offset) {
            if w > way {
                cursor.way = Some(w);
                return Vec::new();
            }
            let start = cursor.offset + 12;
            let next = start + n * 8;
            if w == way {
                let pts = self.bytes()[start..next]
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| {
                        let lon = i32::from_le_bytes(c[0..4].try_into().unwrap());
                        let lat = i32::from_le_bytes(c[4..8].try_into().unwrap());
                        LonLat::new(from_e7(lon), from_e7(lat))
                    })
                    .collect();
                cursor.offset = next;
                cursor.way = self.header(next).map(|(w, _)| w);
                return pts;
            }
            cursor.offset = next;
            cursor.way = self.header(next).map(|(w, _)| w);
        }
        Vec::new()
    }
}

impl Drop for WayGeoms {
    fn drop(&mut self) {
        self.map = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_refs_with_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let mut refs = RefSink::new(dir.path());
        // Way 7 uses nodes 3, 1, 3 (closed); way 5 uses 2 and a missing node 9.
        refs.push_way(7, &[3, 1, 3]).unwrap();
        refs.push_way(5, &[2, 9]).unwrap();
        // Way 6 has only missing nodes.
        refs.push_way(6, &[8]).unwrap();
        assert_eq!(refs.references(), 6);
        let mut j = refs.into_joiner().unwrap();
        for (id, lon) in [(1, 10), (2, 20), (3, 30), (4, 40)] {
            j.node(id, lon, -lon).unwrap();
        }
        assert!(j.node(4, 0, 0).is_err(), "unsorted input is rejected");
        let (geoms, matched) = j.finish(&dir.path().join("geoms")).unwrap();
        assert_eq!(matched, 4);
        let ll = |lon: i32| LonLat::new(from_e7(lon), from_e7(-lon));
        let mut c = WayCursor::default();
        assert_eq!(geoms.line(&mut c, 5), vec![ll(20)]);
        assert!(geoms.line(&mut c, 6).is_empty());
        assert_eq!(geoms.line(&mut c, 7), vec![ll(30), ll(10), ll(30)]);
        // Jumping backwards works too.
        assert_eq!(geoms.line(&mut c, 5), vec![ll(20)]);
        assert!(geoms.line(&mut WayCursor::default(), 1).is_empty());
        assert!(geoms.line(&mut c, 99).is_empty());
    }

    #[test]
    fn coordinate_packing() {
        for (lon, lat) in [(0, 0), (-1_800_000_000, 900_000_000), (1_799_999_999, -899_999_999)] {
            assert_eq!(unpack_coord(pack_coord(lon, lat)), (lon, lat));
        }
    }
}
