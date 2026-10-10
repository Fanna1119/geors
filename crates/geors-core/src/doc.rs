//! Compact binary encoding of [`Place`] documents (postcard, varints).
//!
//! Two variants share one layout:
//! - **stored** (`docs.bin`): fields kept in the fixed-size record (centre,
//!   importance, layer) are omitted, `(osm_key, osm_value)` is an index into
//!   the partition's tag table, and the country code is only stored when it
//!   differs from the partition's.
//! - **full** (import spill files): self-contained.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::geom::{BBox, LonLat, from_e7, to_e7};
use crate::layer::Layer;
use crate::model::{OsmType, Place};

#[derive(Serialize, Deserialize)]
struct Core {
    osm_type: u8,
    osm_id: i64,
    name: Option<String>,
    names: Vec<(String, String)>,
    alt_names: Vec<String>,
    housenumber: Option<String>,
    street: Option<String>,
    postcode: Option<String>,
    city: Option<String>,
    parents: Vec<u32>,
    country_code: Option<String>,
    extent: Option<[i32; 4]>,
    merged_ids: Vec<i64>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    tag: u32,
    core: Core,
}

#[derive(Serialize, Deserialize)]
struct Full {
    osm_key: String,
    osm_value: String,
    layer: u8,
    importance: f32,
    lon: f64,
    lat: f64,
    core: Core,
}

#[derive(Debug, thiserror::Error)]
pub enum DocError {
    #[error("invalid document encoding: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("invalid document: {0}")]
    Invalid(&'static str),
}

fn osm_type_code(t: OsmType) -> u8 {
    match t {
        OsmType::Node => 0,
        OsmType::Way => 1,
        OsmType::Relation => 2,
    }
}

fn osm_type_from(c: u8) -> Result<OsmType, DocError> {
    Ok(match c {
        0 => OsmType::Node,
        1 => OsmType::Way,
        2 => OsmType::Relation,
        _ => return Err(DocError::Invalid("osm type")),
    })
}

fn core_of(p: &Place, implied_cc: Option<&str>) -> Core {
    let cc = p
        .country_code
        .clone()
        .filter(|c| Some(c.as_str()) != implied_cc);
    Core {
        osm_type: osm_type_code(p.osm_type),
        osm_id: p.osm_id,
        name: p.name.clone(),
        names: p
            .names
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        alt_names: p.alt_names.clone(),
        housenumber: p.housenumber.clone(),
        street: p.street.clone(),
        postcode: p.postcode.clone(),
        city: p.city.clone(),
        parents: p.parents.clone(),
        country_code: cc,
        extent: p.extent.map(|e| {
            [
                to_e7(e.min_lon),
                to_e7(e.min_lat),
                to_e7(e.max_lon),
                to_e7(e.max_lat),
            ]
        }),
        merged_ids: p.merged_ids.clone(),
    }
}

/// Values that are not part of the encoded core.
struct Outer {
    osm_key: String,
    osm_value: String,
    layer: Layer,
    importance: f32,
    center: LonLat,
}

fn place_of(c: Core, o: Outer, implied_cc: Option<&str>) -> Result<Place, DocError> {
    Ok(Place {
        osm_type: osm_type_from(c.osm_type)?,
        osm_id: c.osm_id,
        osm_key: o.osm_key,
        osm_value: o.osm_value,
        layer: o.layer,
        name: c.name,
        names: c.names.into_iter().collect::<BTreeMap<_, _>>(),
        alt_names: c.alt_names,
        housenumber: c.housenumber,
        street: c.street,
        postcode: c.postcode,
        city: c.city,
        parents: c.parents,
        country_code: c.country_code.or_else(|| implied_cc.map(str::to_string)),
        center: o.center,
        extent: c
            .extent
            .map(|e| BBox::new(from_e7(e[0]), from_e7(e[1]), from_e7(e[2]), from_e7(e[3]))),
        importance: o.importance,
        lines: Vec::new(),
        polygons: Vec::new(),
        merged_ids: c.merged_ids,
    })
}

/// Self-contained encoding (geometry excluded), for spill files.
pub fn encode_full(p: &Place, out: &mut Vec<u8>) -> Result<(), DocError> {
    let full = Full {
        osm_key: p.osm_key.clone(),
        osm_value: p.osm_value.clone(),
        layer: p.layer as u8,
        importance: p.importance,
        lon: p.center.lon,
        lat: p.center.lat,
        core: core_of(p, None),
    };
    let bytes = postcard::to_extend(&full, std::mem::take(out))?;
    *out = bytes;
    Ok(())
}

pub fn decode_full(bytes: &[u8]) -> Result<Place, DocError> {
    let f: Full = postcard::from_bytes(bytes)?;
    let outer = Outer {
        osm_key: f.osm_key,
        osm_value: f.osm_value,
        layer: Layer::from_u8(f.layer).ok_or(DocError::Invalid("layer"))?,
        importance: f.importance,
        center: LonLat::new(f.lon, f.lat),
    };
    place_of(f.core, outer, None)
}

/// Interns `(osm_key, osm_value)` pairs while writing a partition.
#[derive(Default)]
pub struct TagTable {
    pub pairs: Vec<(String, String)>,
    index: HashMap<(String, String), u32>,
}

impl TagTable {
    pub fn intern(&mut self, key: &str, value: &str) -> u32 {
        let k = (key.to_string(), value.to_string());
        if let Some(&i) = self.index.get(&k) {
            return i;
        }
        self.pairs.push(k.clone());
        let i = (self.pairs.len() - 1) as u32;
        self.index.insert(k, i);
        i
    }
}

/// Partition encoding for `docs.bin`. `partition_cc` is the uppercase
/// country code that is implied for every document.
pub fn encode_stored(
    p: &Place,
    tags: &mut TagTable,
    partition_cc: &str,
    out: &mut Vec<u8>,
) -> Result<(), DocError> {
    let stored = Stored {
        tag: tags.intern(&p.osm_key, &p.osm_value),
        core: core_of(p, Some(partition_cc)),
    };
    let bytes = postcard::to_extend(&stored, std::mem::take(out))?;
    *out = bytes;
    Ok(())
}

/// Inverse of [`encode_stored`]; the rest comes from the place record.
pub fn decode_stored(
    bytes: &[u8],
    tags: &[(String, String)],
    partition_cc: &str,
    layer: Layer,
    importance: f32,
    center: LonLat,
) -> Result<Place, DocError> {
    let s: Stored = postcard::from_bytes(bytes)?;
    let (k, v) = tags
        .get(s.tag as usize)
        .ok_or(DocError::Invalid("tag index"))?;
    let outer = Outer {
        osm_key: k.clone(),
        osm_value: v.clone(),
        layer,
        importance,
        center,
    };
    place_of(s.core, outer, Some(partition_cc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place() -> Place {
        let mut names = BTreeMap::new();
        names.insert("ru".into(), "Вадуц".into());
        Place {
            osm_type: OsmType::Way,
            osm_id: 28_712_148,
            osm_key: "tourism".into(),
            osm_value: "museum".into(),
            layer: Layer::Poi,
            name: Some("Kunstmuseum".into()),
            names,
            alt_names: vec!["KML".into()],
            housenumber: Some("32".into()),
            street: Some("Städtle".into()),
            postcode: Some("9490".into()),
            city: None,
            parents: vec![3, 1],
            country_code: Some("LI".into()),
            center: LonLat::new(9.5221112, 47.1394963),
            extent: Some(BBox::new(9.5217145, 47.1393789, 9.522507, 47.1396136)),
            importance: 0.3,
            lines: Vec::new(),
            polygons: Vec::new(),
            merged_ids: vec![5, 6],
        }
    }

    #[test]
    fn full_roundtrip() {
        let p = place();
        let mut buf = Vec::new();
        encode_full(&p, &mut buf).unwrap();
        assert_eq!(decode_full(&buf).unwrap(), p);
    }

    #[test]
    fn stored_roundtrip_and_size() {
        let p = place();
        let mut tags = TagTable::default();
        let mut buf = Vec::new();
        encode_stored(&p, &mut tags, "LI", &mut buf).unwrap();
        let back = decode_stored(&buf, &tags.pairs, "LI", p.layer, p.importance, p.center).unwrap();
        assert_eq!(back, p);
        let json = serde_json::to_vec(&p).unwrap();
        assert!(
            buf.len() * 3 < json.len(),
            "{} vs {}",
            buf.len(),
            json.len()
        );
        // A foreign country code survives.
        let mut q = p.clone();
        q.country_code = Some("AT".into());
        buf.clear();
        encode_stored(&q, &mut tags, "LI", &mut buf).unwrap();
        let back = decode_stored(&buf, &tags.pairs, "LI", q.layer, q.importance, q.center).unwrap();
        assert_eq!(back.country_code.as_deref(), Some("AT"));
        assert_eq!(tags.pairs.len(), 1);
    }
}
