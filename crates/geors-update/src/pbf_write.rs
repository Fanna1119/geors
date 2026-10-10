//! Minimal OSM PBF writer: header block plus dense-node / way / relation
//! blocks, zlib compressed. No metadata (versions, users): the geocoder
//! does not use it, and replication diffs are applied by id.
//!
//! Format reference: <https://wiki.openstreetmap.org/wiki/PBF_Format>.

use std::collections::HashMap;
use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use crate::osm::{Elem, Kind};

/// Elements per block, as written by osmium.
pub const BLOCK_SIZE: usize = 8000;

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn key(out: &mut Vec<u8>, field: u32, wire: u8) {
    varint(out, ((field as u64) << 3) | wire as u64);
}

fn bytes_field(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    key(out, field, 2);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn uint_field(out: &mut Vec<u8>, field: u32, v: u64) {
    key(out, field, 0);
    varint(out, v);
}

/// Packed repeated varints (already zigzagged where needed).
fn packed(out: &mut Vec<u8>, field: u32, values: impl IntoIterator<Item = u64>) {
    let mut buf = Vec::new();
    for v in values {
        varint(&mut buf, v);
    }
    if !buf.is_empty() {
        bytes_field(out, field, &buf);
    }
}

fn deltas(values: impl IntoIterator<Item = i64>) -> Vec<u64> {
    let mut prev = 0i64;
    values
        .into_iter()
        .map(|v| {
            let d = v - prev;
            prev = v;
            zigzag(d)
        })
        .collect()
}

/// Replication state stored in the PBF header (osmosis conventions).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Replication {
    pub base_url: String,
    pub sequence: u64,
    /// Unix seconds.
    pub timestamp: i64,
}

/// Raw (uncompressed) HeaderBlock.
pub fn header_block(replication: Option<&Replication>) -> Vec<u8> {
    let mut b = Vec::new();
    bytes_field(&mut b, 4, b"OsmSchema-V0.6");
    bytes_field(&mut b, 4, b"DenseNodes");
    bytes_field(&mut b, 5, b"Sort.Type_then_ID");
    bytes_field(
        &mut b,
        16,
        concat!("geors/", env!("CARGO_PKG_VERSION")).as_bytes(),
    );
    if let Some(r) = replication {
        uint_field(&mut b, 32, r.timestamp as u64);
        uint_field(&mut b, 33, r.sequence);
        bytes_field(&mut b, 34, r.base_url.as_bytes());
    }
    b
}

/// String table for one block; index 0 is the required empty string.
#[derive(Default)]
struct Strings {
    list: Vec<String>,
    index: HashMap<String, u32>,
}

impl Strings {
    fn id(&mut self, s: &str) -> u32 {
        if self.list.is_empty() {
            self.list.push(String::new());
        }
        if let Some(&i) = self.index.get(s) {
            return i;
        }
        self.list.push(s.to_string());
        let i = (self.list.len() - 1) as u32;
        self.index.insert(s.to_string(), i);
        i
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        bytes_field(&mut out, 1, b"");
        for s in self.list.iter().skip(1) {
            bytes_field(&mut out, 1, s.as_bytes());
        }
        out
    }
}

/// Raw PrimitiveBlock for elements of one kind, in id order.
pub fn primitive_block(elems: &[Elem]) -> Vec<u8> {
    let mut strings = Strings::default();
    strings.id("");
    let mut group = Vec::new();
    match elems.first().map(Elem::kind) {
        None => {}
        Some(Kind::Node) => {
            let (mut ids, mut lats, mut lons, mut kv) = (vec![], vec![], vec![], vec![]);
            for e in elems {
                if let Elem::Node { id, lon, lat, tags } = e {
                    ids.push(*id);
                    lats.push(*lat as i64);
                    lons.push(*lon as i64);
                    for (k, v) in tags {
                        kv.push(strings.id(k) as u64);
                        kv.push(strings.id(v) as u64);
                    }
                    kv.push(0);
                }
            }
            let mut dense = Vec::new();
            packed(&mut dense, 1, deltas(ids));
            packed(&mut dense, 8, deltas(lats));
            packed(&mut dense, 9, deltas(lons));
            packed(&mut dense, 10, kv);
            bytes_field(&mut group, 2, &dense);
        }
        Some(Kind::Way) => {
            for e in elems {
                if let Elem::Way { id, refs, tags } = e {
                    let mut w = Vec::new();
                    uint_field(&mut w, 1, *id as u64);
                    packed(
                        &mut w,
                        2,
                        tags.iter()
                            .map(|(k, _)| strings.id(k) as u64)
                            .collect::<Vec<_>>(),
                    );
                    packed(
                        &mut w,
                        3,
                        tags.iter()
                            .map(|(_, v)| strings.id(v) as u64)
                            .collect::<Vec<_>>(),
                    );
                    packed(&mut w, 8, deltas(refs.iter().copied()));
                    bytes_field(&mut group, 3, &w);
                }
            }
        }
        Some(Kind::Relation) => {
            for e in elems {
                if let Elem::Relation { id, members, tags } = e {
                    let mut r = Vec::new();
                    uint_field(&mut r, 1, *id as u64);
                    packed(
                        &mut r,
                        2,
                        tags.iter()
                            .map(|(k, _)| strings.id(k) as u64)
                            .collect::<Vec<_>>(),
                    );
                    packed(
                        &mut r,
                        3,
                        tags.iter()
                            .map(|(_, v)| strings.id(v) as u64)
                            .collect::<Vec<_>>(),
                    );
                    packed(
                        &mut r,
                        8,
                        members
                            .iter()
                            .map(|m| strings.id(&m.role) as u64)
                            .collect::<Vec<_>>(),
                    );
                    packed(&mut r, 9, deltas(members.iter().map(|m| m.id)));
                    packed(
                        &mut r,
                        10,
                        members.iter().map(|m| m.kind as u64).collect::<Vec<_>>(),
                    );
                    bytes_field(&mut group, 4, &r);
                }
            }
        }
    }
    let mut block = Vec::new();
    bytes_field(&mut block, 1, &strings.encode());
    if !group.is_empty() {
        bytes_field(&mut block, 2, &group);
    }
    block
}

/// A complete file blob (length prefix, BlobHeader, zlib Blob) ready to be
/// appended to a `.osm.pbf` file. CPU heavy: call it from worker threads.
pub fn blob(kind: &str, raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::new(6));
    z.write_all(raw)?;
    let compressed = z.finish()?;
    let mut blob = Vec::new();
    uint_field(&mut blob, 2, raw.len() as u64);
    bytes_field(&mut blob, 3, &compressed);
    let mut header = Vec::new();
    bytes_field(&mut header, 1, kind.as_bytes());
    uint_field(&mut header, 3, blob.len() as u64);
    let mut out = Vec::with_capacity(4 + header.len() + blob.len());
    out.extend_from_slice(&(header.len() as u32).to_be_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&blob);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osm::Member;

    fn sample() -> Vec<Vec<Elem>> {
        let t = |k: &str, v: &str| (k.to_string(), v.to_string());
        vec![
            vec![
                Elem::Node {
                    id: 1,
                    lon: 95_000_000,
                    lat: 471_000_000,
                    tags: vec![],
                },
                Elem::Node {
                    id: 5,
                    lon: -1_234_567,
                    lat: -471_000_000,
                    tags: vec![t("name", "Städtle"), t("amenity", "cafe")],
                },
            ],
            vec![Elem::Way {
                id: 10,
                refs: vec![1, 5, 1],
                tags: vec![t("highway", "residential")],
            }],
            vec![Elem::Relation {
                id: 20,
                members: vec![
                    Member {
                        kind: Kind::Way,
                        id: 10,
                        role: "outer".into(),
                    },
                    Member {
                        kind: Kind::Node,
                        id: 5,
                        role: String::new(),
                    },
                ],
                tags: vec![t("type", "multipolygon")],
            }],
        ]
    }

    #[test]
    fn roundtrip_through_osmpbf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.osm.pbf");
        let rep = Replication {
            base_url: "https://x/y-updates".into(),
            sequence: 42,
            timestamp: 1_700_000_000,
        };
        let mut file = blob("OSMHeader", &header_block(Some(&rep))).unwrap();
        for group in sample() {
            file.extend(blob("OSMData", &primitive_block(&group)).unwrap());
        }
        std::fs::write(&path, file).unwrap();

        let mut read = Vec::new();
        osmpbf::ElementReader::from_path(&path)
            .unwrap()
            .for_each(|el| read.push(Elem::from_pbf(&el).unwrap()))
            .unwrap();
        assert_eq!(read, sample().concat());

        let mut reader = osmpbf::BlobReader::from_path(&path).unwrap();
        let header = reader.next().unwrap().unwrap().to_headerblock().unwrap();
        assert_eq!(header.osmosis_replication_sequence_number(), Some(42));
        assert_eq!(
            header.osmosis_replication_base_url(),
            Some("https://x/y-updates")
        );
        assert_eq!(header.osmosis_replication_timestamp(), Some(1_700_000_000));
    }
}
