//! A loaded, read-only country partition. All large files are memory mapped.

use std::fs::File;
use std::path::{Path, PathBuf};

use geo_index::rtree::{NeighborsOptions, RTreeIndex, RTreeRef, SimpleDistanceMetric};
use geors_core::geom::{self, LonLat};
use geors_core::storage::{self, FORMAT_VERSION, PartitionMeta, PlaceRecord};
use geors_core::{AdminUnit, BBox, Place};
use memmap2::Mmap;

use crate::IndexError;
use crate::text::TextIndex;

/// A memory map that tolerates empty files (which cannot be mapped).
enum Blob {
    Mapped(Mmap),
    Empty,
}

impl Blob {
    fn open(path: &Path) -> Result<Self, IndexError> {
        let file = File::open(path)
            .map_err(|e| IndexError::Format(format!("cannot open {}: {e}", path.display())))?;
        if file.metadata()?.len() == 0 {
            return Ok(Blob::Empty);
        }
        // SAFETY: partition files are written once and never modified in
        // place; imports swap whole directories.
        Ok(Blob::Mapped(unsafe { Mmap::map(&file)? }))
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Blob::Mapped(m) => m,
            Blob::Empty => &[],
        }
    }
}

impl AsRef<[u8]> for Blob {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

pub struct Partition {
    pub meta: PartitionMeta,
    pub dir: PathBuf,
    pub admins: Vec<AdminUnit>,
    pub text: TextIndex,
    records: Blob,
    docs: Blob,
    geom: Blob,
    spatial: Blob,
}

impl Partition {
    pub fn open(dir: &Path) -> Result<Self, IndexError> {
        let meta_path = dir.join(storage::META_FILE);
        let meta: PartitionMeta =
            serde_json::from_slice(&std::fs::read(&meta_path).map_err(|e| {
                IndexError::Format(format!("{} is not a partition ({e})", dir.display()))
            })?)?;
        if meta.format_version != FORMAT_VERSION {
            return Err(IndexError::Format(format!(
                "partition '{}' uses format v{} but this build expects v{}; re-run `geors import`",
                meta.country_code, meta.format_version, FORMAT_VERSION
            )));
        }
        let admins = serde_json::from_slice(&std::fs::read(dir.join(storage::ADMIN_FILE))?)?;
        let p = Self {
            text: TextIndex::open(&dir.join(storage::TEXT_DIR))?,
            records: Blob::open(&dir.join(storage::PLACES_FILE))?,
            docs: Blob::open(&dir.join(storage::DOCS_FILE))?,
            geom: Blob::open(&dir.join(storage::GEOM_FILE))?,
            spatial: Blob::open(&dir.join(storage::SPATIAL_FILE))?,
            dir: dir.to_path_buf(),
            admins,
            meta,
        };
        let expected = p.meta.num_places as usize * PlaceRecord::SIZE;
        if p.records.bytes().len() != expected {
            return Err(IndexError::Format(format!(
                "partition '{}' is corrupt: {} has {} bytes, expected {expected}",
                p.meta.country_code,
                storage::PLACES_FILE,
                p.records.bytes().len()
            )));
        }
        let tree = p.rtree()?;
        if tree.num_items() != p.meta.num_places {
            return Err(IndexError::Format(format!(
                "partition '{}' is corrupt: spatial index has {} items, expected {}",
                p.meta.country_code,
                tree.num_items(),
                p.meta.num_places
            )));
        }
        Ok(p)
    }

    pub fn code(&self) -> &str {
        &self.meta.country_code
    }

    pub fn len(&self) -> u32 {
        self.meta.num_places
    }

    pub fn is_empty(&self) -> bool {
        self.meta.num_places == 0
    }

    fn rtree(&self) -> Result<RTreeRef<'_, f64>, IndexError> {
        RTreeRef::try_new(&self.spatial)
            .map_err(|e| IndexError::Format(format!("spatial index: {e}")))
    }

    pub fn record(&self, id: u32) -> Option<PlaceRecord> {
        let start = id as usize * PlaceRecord::SIZE;
        PlaceRecord::decode(self.records.bytes().get(start..start + PlaceRecord::SIZE)?)
    }

    pub fn doc(&self, rec: &PlaceRecord) -> Result<Place, IndexError> {
        let start = rec.doc_offset as usize;
        let bytes = self
            .docs
            .bytes()
            .get(start..start + rec.doc_len as usize)
            .ok_or_else(|| IndexError::Format("document offset out of range".into()))?;
        let mut place: Place = serde_json::from_slice(bytes)?;
        place.lines = self.lines(rec);
        Ok(place)
    }

    pub fn lines(&self, rec: &PlaceRecord) -> Vec<Vec<LonLat>> {
        if rec.geom_len == 0 {
            return Vec::new();
        }
        let start = rec.geom_offset as usize;
        let end = start + rec.geom_len as usize * 4;
        self.geom
            .bytes()
            .get(start..end)
            .map(storage::decode_lines)
            .unwrap_or_default()
    }

    /// Exact distance from `p` to the place: to its line geometry when it has
    /// one (streets), otherwise to its centre.
    pub fn distance(&self, rec: &PlaceRecord, p: LonLat) -> f64 {
        if rec.geom_len > 0 {
            let d = self
                .lines(rec)
                .iter()
                .filter_map(|l| geom::point_line_distance(p, l))
                .min_by(f64::total_cmp);
            if let Some(d) = d {
                return d;
            }
        }
        geom::haversine(p, rec.center())
    }

    /// Ids of places whose bbox intersects `bbox`.
    pub fn search_bbox(&self, bbox: &BBox) -> Result<Vec<u32>, IndexError> {
        if !bbox.intersects(&self.meta.bbox) {
            return Ok(Vec::new());
        }
        Ok(self
            .rtree()?
            .search(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat))
    }

    /// Up to `k` ids closest to `p` by bbox distance in a local equirectangular
    /// projection. This is an approximation of true distance order (it ignores
    /// line geometry and earth curvature); `Engine` refines it exactly.
    pub fn approx_neighbors(&self, p: LonLat, k: usize) -> Result<Vec<u32>, IndexError> {
        let metric = ScaledPlanar {
            lon_scale: p.lat.to_radians().cos().max(1e-6),
        };
        let options = NeighborsOptions::k(k);
        Ok(self
            .rtree()?
            .neighbors_with_simple_distance(p.lon, p.lat, options, &metric)
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }
}

/// Squared planar distance with longitude shrunk by cos(latitude).
struct ScaledPlanar {
    lon_scale: f64,
}

impl SimpleDistanceMetric<f64> for ScaledPlanar {
    fn distance(&self, x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
        let dx = (x2 - x1) * self.lon_scale;
        let dy = y2 - y1;
        dx * dx + dy * dy
    }

    fn distance_to_bbox(
        &self,
        x: f64,
        y: f64,
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
    ) -> f64 {
        let axis = |v: f64, lo: f64, hi: f64| {
            if v < lo {
                lo - v
            } else if v > hi {
                v - hi
            } else {
                0.0
            }
        };
        let dx = axis(x, min_x, max_x) * self.lon_scale;
        let dy = axis(y, min_y, max_y);
        dx * dx + dy * dy
    }
}
