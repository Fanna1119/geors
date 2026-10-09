//! On-disk layout of a country partition.
//!
//! ```text
//! <data>/<cc>/
//!   meta.json     PartitionMeta (format version, counts, bbox, source)
//!   places.bin    fixed size PlaceRecord per place, indexed by place id
//!   docs.bin      concatenated JSON encoded `Place` documents
//!   geom.bin      line geometry as little-endian i32 (lon_e7, lat_e7) pairs
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
pub const FORMAT_VERSION: u32 = 1;

pub const META_FILE: &str = "meta.json";
pub const PLACES_FILE: &str = "places.bin";
pub const DOCS_FILE: &str = "docs.bin";
pub const GEOM_FILE: &str = "geom.bin";
pub const SPATIAL_FILE: &str = "spatial.idx";
pub const ADMIN_FILE: &str = "admin.json";
pub const TEXT_DIR: &str = "text";

/// Separator between parts of a multi-part line in `geom.bin`.
pub const GEOM_PART_SEPARATOR: i32 = i32::MIN;

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
        })
    }
}

/// Encode multi-part line geometry into the `geom.bin` i32 stream.
pub fn encode_lines(lines: &[Vec<LonLat>], out: &mut Vec<u8>) -> u32 {
    let mut n = 0u32;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(&GEOM_PART_SEPARATOR.to_le_bytes());
            out.extend_from_slice(&GEOM_PART_SEPARATOR.to_le_bytes());
            n += 2;
        }
        for p in line {
            out.extend_from_slice(&to_e7(p.lon).to_le_bytes());
            out.extend_from_slice(&to_e7(p.lat).to_le_bytes());
            n += 2;
        }
    }
    n
}

/// Decode a `geom.bin` slice produced by [`encode_lines`].
pub fn decode_lines(bytes: &[u8]) -> Vec<Vec<LonLat>> {
    let mut lines = vec![Vec::new()];
    for pair in bytes.as_chunks::<8>().0 {
        let lon = i32::from_le_bytes(pair[0..4].try_into().unwrap());
        let lat = i32::from_le_bytes(pair[4..8].try_into().unwrap());
        if lon == GEOM_PART_SEPARATOR {
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
}
