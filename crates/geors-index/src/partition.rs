//! A loaded, read-only country partition. All large files are memory mapped.

use std::fs::File;
use std::path::{Path, PathBuf};

use geo_index::rtree::{NeighborsOptions, RTreeIndex, RTreeRef, SimpleDistanceMetric};
use geors_core::doc;
use geors_core::geom::{self, LonLat};
use geors_core::geom::{f32_down, f32_up};
use geors_core::storage::{self, FORMAT_VERSION, GeomKind, PartitionMeta, PlaceRecord};
use geors_core::{AdminUnit, BBox, Layer, Place};
use memmap2::Mmap;

use crate::IndexError;
use crate::text::TextIndex;

/// A memory map that tolerates empty files (which cannot be mapped).
enum Blob {
    Mapped(Mmap),
    Empty,
}

/// Queries read index files at random. Without this hint, Linux reads
/// ahead around every page fault (up to `read_ahead_kb`, 8 MB in Docker
/// Desktop's VM), so a search touching a few hundred scattered pages read
/// gigabytes and evicted the cache it was building. Only matters once the
/// index is larger than the page cache.
pub(crate) fn advise_random(map: &Mmap) {
    #[cfg(unix)]
    let _ = map.advise(memmap2::Advice::Random);
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
        let map = unsafe { Mmap::map(&file)? };
        advise_random(&map);
        Ok(Blob::Mapped(map))
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Blob::Mapped(m) => m,
            Blob::Empty => &[],
        }
    }
}

/// Small per-place files copied out of the records, so that scoring text
/// matches touches a fraction of `places.bin` (see `storage`): file name
/// and bytes per place. Imports write them; partitions written before they
/// existed fall back to the records, and `geors upgrade` adds them.
const DERIVED: [(&str, usize); 2] = [(storage::IMPORTANCE_FILE, 4), (storage::CENTERS_FILE, 8)];

/// The value a derived file stores for `rec`.
fn derived_value(file: &str, rec: &PlaceRecord, out: &mut Vec<u8>) {
    if file == storage::IMPORTANCE_FILE {
        out.extend_from_slice(&rec.importance.to_le_bytes());
    } else {
        out.extend_from_slice(&rec.lon_e7.to_le_bytes());
        out.extend_from_slice(&rec.lat_e7.to_le_bytes());
    }
}

/// A derived file, or `None` if missing or of the wrong size (ignored, as
/// the records have the same values).
fn open_derived(dir: &Path, file: &str, num_places: u32) -> Result<Option<Blob>, IndexError> {
    let path = dir.join(file);
    if !path.exists() {
        return Ok(None);
    }
    let (_, size) = DERIVED
        .iter()
        .find(|(f, _)| *f == file)
        .expect("derived file");
    let blob = Blob::open(&path)?;
    if blob.bytes().len() != num_places as usize * size {
        tracing::warn!("ignoring {}: wrong size", path.display());
        return Ok(None);
    }
    Ok(Some(blob))
}

/// Write the derived files a partition lacks, from its records (one
/// sequential pass). Returns the names of the files written.
pub fn add_derived_files(dir: &Path) -> Result<Vec<&'static str>, IndexError> {
    let missing: Vec<&'static str> = DERIVED
        .iter()
        .map(|(f, _)| *f)
        .filter(|f| !dir.join(f).exists())
        .collect();
    if missing.is_empty() {
        return Ok(missing);
    }
    let records = Blob::open(&dir.join(storage::PLACES_FILE))?;
    let tmp = |f: &str| dir.join(format!(".{f}.tmp-{}", std::process::id()));
    let mut outs = missing
        .iter()
        .map(|f| {
            Ok(std::io::BufWriter::with_capacity(
                1 << 20,
                File::create(tmp(f))?,
            ))
        })
        .collect::<Result<Vec<_>, IndexError>>()?;
    let mut buf = Vec::new();
    for chunk in records.bytes().chunks_exact(PlaceRecord::SIZE) {
        let rec = PlaceRecord::decode(chunk)
            .ok_or_else(|| IndexError::Format(format!("bad record in {}", dir.display())))?;
        for (f, out) in missing.iter().zip(&mut outs) {
            buf.clear();
            derived_value(f, &rec, &mut buf);
            std::io::Write::write_all(out, &buf)?;
        }
    }
    for (f, mut out) in missing.iter().zip(outs) {
        std::io::Write::flush(&mut out)?;
        drop(out);
        std::fs::rename(tmp(f), dir.join(f))?;
    }
    Ok(missing)
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
    /// `importance.bin` and `centers.bin`, if present with the right size.
    importance: Option<Blob>,
    centers: Option<Blob>,
    docs: Blob,
    geom: Blob,
    spatial: Blob,
    /// Areas (places with a polygon, layers locality..country) indexed by
    /// their polygon's extent, built on first use (`/contains`). The main
    /// R-tree holds only centres and lines, so it cannot answer "which
    /// areas contain this point".
    areas: std::sync::OnceLock<Option<AreaTree>>,
}

/// See [`Partition::areas_at`].
struct AreaTree {
    ids: Vec<u32>,
    tree: geo_index::rtree::RTree<f64>,
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
            text: TextIndex::open(&dir.join(storage::TEXT_DIR), &meta.country_code)?,
            records: Blob::open(&dir.join(storage::PLACES_FILE))?,
            importance: open_derived(dir, storage::IMPORTANCE_FILE, meta.num_places)?,
            centers: open_derived(dir, storage::CENTERS_FILE, meta.num_places)?,
            docs: Blob::open(&dir.join(storage::DOCS_FILE))?,
            geom: Blob::open(&dir.join(storage::GEOM_FILE))?,
            spatial: Blob::open(&dir.join(storage::SPATIAL_FILE))?,
            areas: std::sync::OnceLock::new(),
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

    /// Importance of place `id`: from `importance.bin` when the partition
    /// has one, else from its record.
    pub fn importance(&self, id: u32) -> Option<f32> {
        match &self.importance {
            Some(blob) => {
                let start = id as usize * 4;
                let b = blob.bytes().get(start..start + 4)?;
                Some(f32::from_le_bytes(b.try_into().ok()?))
            }
            None => self.record(id).map(|r| r.importance),
        }
    }

    /// Centre of place `id`: from `centers.bin` when the partition has one,
    /// else from its record.
    pub fn center(&self, id: u32) -> Option<LonLat> {
        match &self.centers {
            Some(blob) => {
                let start = id as usize * 8;
                let b = blob.bytes().get(start..start + 8)?;
                let lon = i32::from_le_bytes(b[0..4].try_into().ok()?);
                let lat = i32::from_le_bytes(b[4..8].try_into().ok()?);
                Some(LonLat::new(geom::from_e7(lon), geom::from_e7(lat)))
            }
            None => self.record(id).map(|r| r.center()),
        }
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
    /// Areas whose polygon extent contains `p` (candidates for an exact
    /// point-in-polygon test).
    pub fn areas_at(&self, p: geors_core::LonLat) -> Result<Vec<u32>, IndexError> {
        use geo_index::rtree::{RTreeBuilder, RTreeIndex, sort::HilbertSort};
        let areas = self.areas.get_or_init(|| {
            let layers: Vec<Layer> = Layer::ALL
                .into_iter()
                .filter(|l| *l >= Layer::Locality)
                .collect();
            let candidates = self.text.ids_in_layers(&layers).ok()?;
            let mut ids = Vec::new();
            let mut boxes = Vec::new();
            for id in candidates {
                let Some(rec) = self.record(id) else { continue };
                if rec.geom_kind != GeomKind::Polygons {
                    continue;
                }
                // The stored extent; decoding every polygon would take
                // seconds on a large partition.
                let extent = self.doc(&rec, false).ok().and_then(|d| d.extent);
                let b = match extent {
                    Some(b) => b,
                    None => {
                        let mut b = BBox::empty();
                        for point in self.polygons(&rec).iter().flatten().flatten() {
                            b.extend(*point);
                        }
                        b
                    }
                };
                ids.push(id);
                boxes.push(b);
            }
            if ids.is_empty() {
                return None;
            }
            let mut builder = RTreeBuilder::<f64>::new(ids.len() as u32);
            for b in &boxes {
                builder.add(b.min_lon, b.min_lat, b.max_lon, b.max_lat);
            }
            Some(AreaTree {
                ids,
                tree: builder.finish::<HilbertSort>(),
            })
        });
        Ok(areas.as_ref().map_or_else(Vec::new, |a| {
            a.tree
                .search(p.lon, p.lat, p.lon, p.lat)
                .into_iter()
                .map(|i| a.ids[i as usize])
                .collect()
        }))
    }

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
