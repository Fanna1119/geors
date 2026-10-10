//! A loaded, read-only country partition. All large files are memory mapped.

use std::fs::File;
use std::path::{Path, PathBuf};

use geo_index::rtree::{NeighborsOptions, RTreeIndex, RTreeRef, SimpleDistanceMetric};
use geors_core::doc;
use geors_core::geom::{self, LonLat};
use geors_core::geom::{f32_down, f32_up};
use geors_core::storage::{self, FORMAT_VERSION, GeomKind, PartitionMeta, PlaceRecord};
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
    /// (osm_key, osm_value) pairs referenced by documents.
    tags: Vec<(String, String)>,
    /// Uppercase country code implied for documents.
    cc_upper: String,
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
        let tags = serde_json::from_slice(&std::fs::read(dir.join(storage::TAGS_FILE))?)?;
        let p = Self {
            text: TextIndex::open(&dir.join(storage::TEXT_DIR))?,
            records: Blob::open(&dir.join(storage::PLACES_FILE))?,
            docs: Blob::open(&dir.join(storage::DOCS_FILE))?,
            geom: Blob::open(&dir.join(storage::GEOM_FILE))?,
            spatial: Blob::open(&dir.join(storage::SPATIAL_FILE))?,
            dir: dir.to_path_buf(),
            admins,
            tags,
            cc_upper: meta.country_code.to_ascii_uppercase(),
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

    fn rtree(&self) -> Result<RTreeRef<'_, f32>, IndexError> {
        RTreeRef::try_new(&self.spatial)
            .map_err(|e| IndexError::Format(format!("spatial index: {e}")))
    }

    pub fn record(&self, id: u32) -> Option<PlaceRecord> {
        let start = id as usize * PlaceRecord::SIZE;
        PlaceRecord::decode(self.records.bytes().get(start..start + PlaceRecord::SIZE)?)
    }

    /// Load the JSON document. Geometry is attached only if `with_geometry`.
    pub fn doc(&self, rec: &PlaceRecord, with_geometry: bool) -> Result<Place, IndexError> {
        let start = rec.doc_offset as usize;
        let bytes = self
            .docs
            .bytes()
            .get(start..start + rec.doc_len as usize)
            .ok_or_else(|| IndexError::Format("document offset out of range".into()))?;
        let mut place = doc::decode_stored(
            bytes,
            &self.tags,
            &self.cc_upper,
            rec.layer,
            rec.importance,
            rec.center(),
        )
        .map_err(|e| IndexError::Format(format!("partition '{}': {e}", self.code())))?;
        if with_geometry {
            match rec.geom_kind {
                GeomKind::Lines => place.lines = self.lines(rec),
                GeomKind::Polygons => place.polygons = self.polygons(rec),
                GeomKind::None => {}
            }
        }
        Ok(place)
    }

    fn geom_bytes(&self, rec: &PlaceRecord) -> &[u8] {
        let start = rec.geom_offset as usize;
        let end = start + rec.geom_len as usize * 4;
        self.geom.bytes().get(start..end).unwrap_or_default()
    }

    pub fn lines(&self, rec: &PlaceRecord) -> Vec<Vec<LonLat>> {
        if rec.geom_kind != GeomKind::Lines {
            return Vec::new();
        }
        storage::decode_lines(self.geom_bytes(rec))
    }

    pub fn polygons(&self, rec: &PlaceRecord) -> Vec<storage::PolygonRings> {
        if rec.geom_kind != GeomKind::Polygons {
            return Vec::new();
        }
        storage::decode_polygons(self.geom_bytes(rec))
    }

    /// Exact distance from `p` to the place: to its line geometry when it has
    /// one (streets), otherwise to its centre.
    pub fn distance(&self, rec: &PlaceRecord, p: LonLat) -> f64 {
        if rec.geom_kind == GeomKind::Lines
            && let Some(d) = storage::lines_distance(self.geom_bytes(rec), p)
        {
            return d;
        }
        geom::haversine(p, rec.center())
    }

    /// Ids of places whose bbox intersects `bbox`.
    pub fn search_bbox(&self, bbox: &BBox) -> Result<Vec<u32>, IndexError> {
        if !bbox.intersects(&self.meta.bbox) {
            return Ok(Vec::new());
        }
        Ok(self.rtree()?.search(
            f32_down(bbox.min_lon),
            f32_down(bbox.min_lat),
            f32_up(bbox.max_lon),
            f32_up(bbox.max_lat),
        ))
    }

    /// Up to `k` places in increasing order of a *lower bound* on their
    /// distance from `p` (bbox distance in a local equirectangular
    /// projection, in metres, conservatively reduced). Only places whose
    /// bound is at most `max_m` are returned.
    pub fn neighbors_lower_bound(
        &self,
        p: LonLat,
        k: usize,
        max_m: f64,
    ) -> Result<Vec<(u32, f64)>, IndexError> {
        let metric = ScaledPlanar {
            lon_scale: p.lat.to_radians().cos().max(1e-6),
        };
        let mut options = NeighborsOptions::k(k);
        if max_m.is_finite() {
            let deg = (max_m + F32_SLACK_M) / LOWER_BOUND_M_PER_DEG;
            options = options.max_distance((deg * deg) as f32);
        }
        Ok(self
            .rtree()?
            .neighbors_with_simple_distance(p.lon as f32, p.lat as f32, options, &metric)
            .into_iter()
            .map(|(id, sq)| {
                let m = (sq as f64).sqrt() * LOWER_BOUND_M_PER_DEG - F32_SLACK_M;
                (id, m.max(0.0))
            })
            .collect())
    }
}

/// Metres per degree for [`Partition::neighbors_lower_bound`]: the length
/// of a degree of latitude, reduced by 20 % because scaling longitude by
/// the query point's cos(latitude) is only exact at that latitude.
const LOWER_BOUND_M_PER_DEG: f64 = 111_195.0 * 0.8;

/// The tree stores f32 (about 1e-6 degrees); the query point is rounded too.
/// Lower bounds are reduced by this much so they stay lower bounds.
const F32_SLACK_M: f64 = 1.0;

/// Squared planar distance with longitude shrunk by cos(latitude).
struct ScaledPlanar {
    lon_scale: f64,
}

impl SimpleDistanceMetric<f32> for ScaledPlanar {
    fn distance(&self, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
        let dx = (x2 as f64 - x1 as f64) * self.lon_scale;
        let dy = y2 as f64 - y1 as f64;
        (dx * dx + dy * dy) as f32
    }

    fn distance_to_bbox(
        &self,
        x: f32,
        y: f32,
        min_x: f32,
        min_y: f32,
        max_x: f32,
        max_y: f32,
    ) -> f32 {
        let axis = |v: f32, lo: f32, hi: f32| {
            if v < lo {
                (lo - v) as f64
            } else if v > hi {
                (v - hi) as f64
            } else {
                0.0
            }
        };
        let dx = axis(x, min_x, max_x) * self.lon_scale;
        let dy = axis(y, min_y, max_y);
        (dx * dx + dy * dy) as f32
    }
}
