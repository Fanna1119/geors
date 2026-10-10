//! Append-only on-disk buffer of [`Place`]s, used during import so that
//! places never have to be held in memory all at once.
//!
//! Record layout (little endian):
//!
//! ```text
//! u32 record_len (bytes after this field)
//! f64 lon, f64 lat          centre       } readable without decoding the
//! u8 osm_type, i64 osm_id                } document (sorting)
//! u32 doc_len, doc          the Place, `doc::encode_full` (no geometry)
//! u8  geom_kind, u32 n, n * i32   line / polygon geometry (storage format)
//! ```

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::Mmap;

use crate::Place;
use crate::doc;
use crate::geom::LonLat;
use crate::storage::{self, GeomKind};

pub struct SpillWriter {
    out: BufWriter<File>,
    path: PathBuf,
    count: u64,
    offset: u64,
    buf: Vec<u8>,
}

/// A finished spill file.
#[derive(Debug, Clone)]
pub struct Spill {
    pub path: PathBuf,
    pub count: u64,
}

impl SpillWriter {
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            out: BufWriter::with_capacity(1 << 20, File::create(path)?),
            path: path.to_path_buf(),
            count: 0,
            offset: 0,
            buf: Vec::new(),
        })
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Append a place; returns the record's offset.
    pub fn push(&mut self, place: &Place) -> io::Result<u64> {
        let mut doc = Vec::new();
        doc::encode_full(place, &mut doc).map_err(io::Error::other)?;
        let b = &mut self.buf;
        b.clear();
        b.extend_from_slice(&place.center.lon.to_le_bytes());
        b.extend_from_slice(&place.center.lat.to_le_bytes());
        b.push(place.osm_type as u8);
        b.extend_from_slice(&place.osm_id.to_le_bytes());
        b.extend_from_slice(&(doc.len() as u32).to_le_bytes());
        b.extend_from_slice(&doc);
        let mut geom = Vec::new();
        let (kind, n) = if !place.lines.is_empty() {
            (
                GeomKind::Lines,
                storage::encode_lines(&place.lines, &mut geom),
            )
        } else if !place.polygons.is_empty() {
            (
                GeomKind::Polygons,
                storage::encode_polygons(&place.polygons, &mut geom),
            )
        } else {
            (GeomKind::None, 0)
        };
        b.push(kind as u8);
        b.extend_from_slice(&n.to_le_bytes());
        b.extend_from_slice(&geom);

        let offset = self.offset;
        self.out.write_all(&(b.len() as u32).to_le_bytes())?;
        self.out.write_all(b)?;
        self.offset += 4 + b.len() as u64;
        self.count += 1;
        Ok(offset)
    }

    pub fn finish(mut self) -> io::Result<Spill> {
        self.out.flush()?;
        Ok(Spill {
            path: self.path,
            count: self.count,
        })
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("corrupt spill file: {msg}"),
    )
}

/// Index entry of a spill record.
#[derive(Debug, Clone, Copy)]
pub struct SpillEntry {
    pub offset: u64,
    pub center: LonLat,
    /// `OsmType as u8`.
    pub osm_type: u8,
    pub osm_id: i64,
}

/// Read access to a spill file (memory mapped).
pub struct SpillReader {
    map: Option<Mmap>,
}

impl SpillReader {
    pub fn open(spill: &Spill) -> io::Result<Self> {
        let file = File::open(&spill.path)?;
        let map = if file.metadata()?.len() == 0 {
            None
        } else {
            // SAFETY: spill files are private to this import and not modified
            // once finished.
            Some(unsafe { Mmap::map(&file)? })
        };
        Ok(Self { map })
    }

    fn bytes(&self) -> &[u8] {
        self.map.as_deref().unwrap_or_default()
    }

    /// Every record's offset plus what is needed to sort it, in file order.
    pub fn index(&self) -> io::Result<Vec<SpillEntry>> {
        let mut out = Vec::new();
        self.for_each_entry(|e| out.push(e))?;
        Ok(out)
    }

    /// Like [`index`](Self::index) without materialising the list (large
    /// partitions: tens of millions of entries).
    pub fn for_each_entry(&self, mut f: impl FnMut(SpillEntry)) -> io::Result<()> {
        let b = self.bytes();
        let mut pos = 0usize;
        while pos < b.len() {
            let len = read_u32(b, pos)? as usize;
            let lon = f64::from_le_bytes(slice(b, pos + 4, 8)?.try_into().unwrap());
            let lat = f64::from_le_bytes(slice(b, pos + 12, 8)?.try_into().unwrap());
            let osm_type = *slice(b, pos + 20, 1)?.first().unwrap();
            let osm_id = i64::from_le_bytes(slice(b, pos + 21, 8)?.try_into().unwrap());
            f(SpillEntry {
                offset: pos as u64,
                center: LonLat::new(lon, lat),
                osm_type,
                osm_id,
            });
            pos += 4 + len;
        }
        Ok(())
    }

    /// Decode the record at `offset`, including its geometry.
    pub fn get(&self, offset: u64) -> io::Result<Place> {
        let b = self.bytes();
        let pos = offset as usize;
        let doc_len = read_u32(b, pos + 29)? as usize;
        let doc = slice(b, pos + 33, doc_len)?;
        let mut place = doc::decode_full(doc).map_err(io::Error::other)?;
        let g = pos + 33 + doc_len;
        let kind = GeomKind::from_u8(*b.get(g).ok_or_else(|| bad("geometry kind"))?)
            .ok_or_else(|| bad("geometry kind"))?;
        let n = read_u32(b, g + 1)? as usize;
        let geom = slice(b, g + 5, n * 4)?;
        match kind {
            GeomKind::Lines => place.lines = storage::decode_lines(geom),
            GeomKind::Polygons => place.polygons = storage::decode_polygons(geom),
            GeomKind::None => {}
        }
        Ok(place)
    }
}

fn slice(b: &[u8], start: usize, len: usize) -> io::Result<&[u8]> {
    b.get(start..start + len)
        .ok_or_else(|| bad("truncated record"))
}

fn read_u32(b: &[u8], pos: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(slice(b, pos, 4)?.try_into().unwrap()))
}

/// A scratch directory removed (with its contents) when dropped.
pub struct WorkDir {
    pub path: PathBuf,
}

impl WorkDir {
    pub fn create(parent: &Path, prefix: &str) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{prefix}-{}-{n}", std::process::id()));
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Layer, OsmType};

    fn place(id: i64) -> Place {
        Place {
            osm_type: OsmType::Way,
            osm_id: id,
            osm_key: "highway".into(),
            osm_value: "residential".into(),
            layer: Layer::Street,
            name: Some(format!("Street {id}")),
            names: Default::default(),
            alt_names: vec![],
            housenumber: None,
            street: None,
            postcode: None,
            city: None,
            parents: vec![1, 2],
            country_code: Some("LI".into()),
            center: LonLat::new(9.5, 47.1),
            extent: None,
            importance: 0.25,
            lines: vec![vec![LonLat::new(9.5, 47.1), LonLat::new(9.6, 47.2)]],
            polygons: Vec::new(),
            merged_ids: vec![7],
        }
    }

    #[test]
    fn roundtrip() {
        let dir = WorkDir::create(&std::env::temp_dir(), "geors-spill-test").unwrap();
        let mut w = SpillWriter::create(&dir.path.join("a.spill")).unwrap();
        let mut offsets = Vec::new();
        for i in 0..100 {
            offsets.push(w.push(&place(i)).unwrap());
        }
        let spill = w.finish().unwrap();
        assert_eq!(spill.count, 100);
        let r = SpillReader::open(&spill).unwrap();
        let index = r.index().unwrap();
        assert_eq!(index.iter().map(|e| e.offset).collect::<Vec<_>>(), offsets);
        assert_eq!(index[42].osm_id, 42);
        let p = r.get(offsets[42]).unwrap();
        assert_eq!(p, place(42));
        let path = dir.path.clone();
        drop(dir);
        assert!(!path.exists());
    }
}
