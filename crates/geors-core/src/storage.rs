//! On-disk layout of a country partition.
//!
//! ```text
//! <data>/<cc>/
//!   meta.json     PartitionMeta (format version, counts, bbox, source)
//!   places.bin    fixed size PlaceRecord per place, indexed by place id
//!   docs.bin      concatenated JSON encoded `Place` documents
//!   geom.bin      line / polygon geometry as little-endian i32 (lon_e7, lat_e7) pairs
//!   spatial.idx   packed Hilbert R-tree (flatbush ABI) over place bboxes
//!   admin.json    admin units referenced by `Place::parents`
//!   text/         tantivy full text index
//! ```
//!
//! Every file except `admin.json` and `meta.json` is memory mapped at runtime,
//! so resident memory stays small and is managed by the OS page cache.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::geom::{BBox, LonLat, from_e7, to_e7};
use crate::layer::Layer;

/// Bump whenever the layout of any partition file changes.
pub const FORMAT_VERSION: u32 = 2;

pub const META_FILE: &str = "meta.json";
pub const PLACES_FILE: &str = "places.bin";
pub const DOCS_FILE: &str = "docs.bin";
pub const GEOM_FILE: &str = "geom.bin";
pub const SPATIAL_FILE: &str = "spatial.idx";
pub const ADMIN_FILE: &str = "admin.json";
pub const TEXT_DIR: &str = "text";

/// `<data>/GENERATION`: changes whenever any partition is (re)written, so a
/// running server can cheaply notice that it should reload.
pub const GENERATION_FILE: &str = "GENERATION";

pub fn read_generation(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(data_dir.join(GENERATION_FILE))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Record that partitions in `data_dir` changed (atomic write).
pub fn bump_generation(data_dir: &Path) -> std::io::Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let value = format!("{}.{:09}", now.as_secs(), now.subsec_nanos());
    let tmp = data_dir.join(format!(".{GENERATION_FILE}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &value)?;
    std::fs::rename(&tmp, data_dir.join(GENERATION_FILE))?;
    Ok(value)
}

/// Marker in the lon slot of `geom.bin`: the lat slot says what follows.
pub const GEOM_SEPARATOR: i32 = i32::MIN;
/// Lat value after [`GEOM_SEPARATOR`]: a new line, or a new polygon.
pub const SEP_PART: i32 = 0;
/// Lat value after [`GEOM_SEPARATOR`]: a new ring (hole) in the current polygon.
pub const SEP_RING: i32 = 1;

/// What `geom.bin` holds for a place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum GeomKind {
    /// Point only (the record's centre).
    #[default]
    None = 0,
    /// One or more polylines (streets, rivers).
    Lines = 1,
    /// One or more polygons with optional holes (areas, buildings, boundaries).
    Polygons = 2,
}

impl GeomKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => GeomKind::None,
            1 => GeomKind::Lines,
            2 => GeomKind::Polygons,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionMeta {
    pub format_version: u32,
    pub country_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_name: Option<String>,
    pub num_places: u32,
    pub bbox: BBox,
    pub layers: BTreeMap<String, u32>,
    pub source: String,
    pub created_unix: u64,
}

pub fn partition_dir(data_dir: &Path, country_code: &str) -> PathBuf {
    data_dir.join(country_code)
}

/// Fixed size per-place record. Everything needed for spatial filtering and
/// ranking without touching the (larger) JSON document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaceRecord {
    pub lon_e7: i32,
    pub lat_e7: i32,
    pub doc_offset: u64,
    pub doc_len: u32,
    pub geom_offset: u64,
    /// Number of i32 values in `geom.bin`; 0 means "point only".
    pub geom_len: u32,
    pub importance: f32,
    pub layer: Layer,
    pub geom_kind: GeomKind,
}

impl PlaceRecord {
    pub const SIZE: usize = 40;

    pub fn center(&self) -> LonLat {
        LonLat::new(from_e7(self.lon_e7), from_e7(self.lat_e7))
    }

    pub fn set_center(&mut self, p: LonLat) {
        self.lon_e7 = to_e7(p.lon);
        self.lat_e7 = to_e7(p.lat);
    }

    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.lon_e7.to_le_bytes());
        b[4..8].copy_from_slice(&self.lat_e7.to_le_bytes());
        b[8..16].copy_from_slice(&self.doc_offset.to_le_bytes());
        b[16..20].copy_from_slice(&self.doc_len.to_le_bytes());
        b[20..28].copy_from_slice(&self.geom_offset.to_le_bytes());
        b[28..32].copy_from_slice(&self.geom_len.to_le_bytes());
        b[32..36].copy_from_slice(&self.importance.to_le_bytes());
        b[36] = self.layer as u8;
        b[37] = self.geom_kind as u8;
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < Self::SIZE {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        Some(Self {
            lon_e7: u32_at(0) as i32,
            lat_e7: u32_at(4) as i32,
            doc_offset: u64_at(8),
            doc_len: u32_at(16),
            geom_offset: u64_at(20),
            geom_len: u32_at(28),
            importance: f32::from_bits(u32_at(32)),
            layer: Layer::from_u8(b[36])?,
            geom_kind: GeomKind::from_u8(b[37])?,
        })
    }
}

fn push_pair(out: &mut Vec<u8>, lon: i32, lat: i32) {
    out.extend_from_slice(&lon.to_le_bytes());
    out.extend_from_slice(&lat.to_le_bytes());
}

fn push_points(out: &mut Vec<u8>, pts: &[LonLat]) {
    for p in pts {
        push_pair(out, to_e7(p.lon), to_e7(p.lat));
    }
}

fn pairs(bytes: &[u8]) -> impl Iterator<Item = (i32, i32)> + '_ {
    bytes.as_chunks::<8>().0.iter().map(|c| {
        (
            i32::from_le_bytes(c[0..4].try_into().unwrap()),
            i32::from_le_bytes(c[4..8].try_into().unwrap()),
        )
    })
}

/// Encode multi-part line geometry into the `geom.bin` i32 stream.
/// Returns the number of i32 values written.
pub fn encode_lines(lines: &[Vec<LonLat>], out: &mut Vec<u8>) -> u32 {
    let start = out.len();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            push_pair(out, GEOM_SEPARATOR, SEP_PART);
        }
        push_points(out, line);
    }
    ((out.len() - start) / 4) as u32
}

/// Decode a `geom.bin` slice produced by [`encode_lines`].
pub fn decode_lines(bytes: &[u8]) -> Vec<Vec<LonLat>> {
    let mut lines = vec![Vec::new()];
    for (lon, lat) in pairs(bytes) {
        if lon == GEOM_SEPARATOR {
            lines.push(Vec::new());
        } else {
            lines
                .last_mut()
                .unwrap()
                .push(LonLat::new(from_e7(lon), from_e7(lat)));
        }
    }
    lines.retain(|l| !l.is_empty());
    lines
}

/// A polygon as rings; the first ring is the exterior, the rest are holes.
pub type PolygonRings = Vec<Vec<LonLat>>;

/// Encode polygons (with holes) into the `geom.bin` i32 stream.
pub fn encode_polygons(polygons: &[PolygonRings], out: &mut Vec<u8>) -> u32 {
    let start = out.len();
    for (i, poly) in polygons.iter().enumerate() {
        if i > 0 {
            push_pair(out, GEOM_SEPARATOR, SEP_PART);
        }
        for (j, ring) in poly.iter().enumerate() {
            if j > 0 {
                push_pair(out, GEOM_SEPARATOR, SEP_RING);
            }
            push_points(out, ring);
        }
    }
    ((out.len() - start) / 4) as u32
}

/// Decode a `geom.bin` slice produced by [`encode_polygons`].
pub fn decode_polygons(bytes: &[u8]) -> Vec<PolygonRings> {
    let mut polys: Vec<PolygonRings> = vec![vec![Vec::new()]];
    for (lon, lat) in pairs(bytes) {
        if lon == GEOM_SEPARATOR {
            if lat == SEP_RING {
                polys.last_mut().unwrap().push(Vec::new());
            } else {
                polys.push(vec![Vec::new()]);
            }
        } else {
            let ring = polys.last_mut().unwrap().last_mut().unwrap();
            ring.push(LonLat::new(from_e7(lon), from_e7(lat)));
        }
    }
    for p in &mut polys {
        p.retain(|r| !r.is_empty());
    }
    polys.retain(|p| !p.is_empty());
    polys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrip() {
        let r = PlaceRecord {
            lon_e7: -1_234_567,
            lat_e7: 471_234_567,
            doc_offset: 1 << 40,
            doc_len: 321,
            geom_offset: 99,
            geom_len: 12,
            importance: 0.75,
            layer: Layer::Street,
            geom_kind: GeomKind::Lines,
        };
        assert_eq!(PlaceRecord::decode(&r.encode()), Some(r));
    }

    #[test]
    fn lines_roundtrip() {
        let lines = vec![
            vec![LonLat::new(9.5, 47.1), LonLat::new(9.51, 47.11)],
            vec![LonLat::new(-0.1, -0.2)],
        ];
        let mut buf = Vec::new();
        let n = encode_lines(&lines, &mut buf);
        assert_eq!(n as usize * 4, buf.len());
        assert_eq!(decode_lines(&buf), lines);
    }

    #[test]
    fn polygons_roundtrip() {
        let square = |o: f64| {
            vec![
                LonLat::new(o, o),
                LonLat::new(o + 1.0, o),
                LonLat::new(o + 1.0, o + 1.0),
                LonLat::new(o, o),
            ]
        };
        let polys = vec![vec![square(0.0), square(0.25)], vec![square(5.0)]];
        let mut buf = Vec::new();
        let n = encode_polygons(&polys, &mut buf);
        assert_eq!(n as usize * 4, buf.len());
        assert_eq!(decode_polygons(&buf), polys);
    }
}
