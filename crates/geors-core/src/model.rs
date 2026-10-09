use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::geom::{BBox, LonLat};
use crate::layer::Layer;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OsmType {
    #[serde(rename = "N")]
    Node,
    #[serde(rename = "W")]
    Way,
    #[serde(rename = "R")]
    Relation,
}

impl OsmType {
    pub fn as_str(self) -> &'static str {
        match self {
            OsmType::Node => "N",
            OsmType::Way => "W",
            OsmType::Relation => "R",
        }
    }
}

/// An administrative or locality unit that places can reference as parent
/// (country, state, county, city, district). Stored once per partition in
/// `admin.json` so that localised names do not have to be repeated on every
/// address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminUnit {
    pub layer: Layer,
    pub name: String,
    /// Localised names keyed by language code (from `name:<lang>`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub names: BTreeMap<String, String>,
}

impl AdminUnit {
    pub fn localized(&self, lang: Option<&str>) -> &str {
        lang.and_then(|l| self.names.get(l)).unwrap_or(&self.name)
    }
}

/// A searchable place: a POI, address, street or administrative unit.
///
/// This is what ingestion produces and what is stored (as JSON) in a
/// partition's `docs.bin`. Line geometry lives separately in `geom.bin` and
/// is therefore not serialised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Place {
    pub osm_type: OsmType,
    pub osm_id: i64,
    pub osm_key: String,
    pub osm_value: String,
    pub layer: Layer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Localised names keyed by language code (from `name:<lang>`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub names: BTreeMap<String, String>,
    /// Alternative, old, short, official names. Searchable but not displayed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alt_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub housenumber: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub street: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postcode: Option<String>,
    /// City from `addr:city` / `addr:place`, used only when no city parent
    /// could be derived from boundaries or nearby place nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    /// Indices into the partition's admin table, any order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
    pub center: LonLat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extent: Option<BBox>,
    pub importance: f32,
    /// Line geometry (streets, rivers) used for exact reverse distances.
    #[serde(skip)]
    pub lines: Vec<Vec<LonLat>>,
    /// Area geometry (first ring exterior, then holes), for output only.
    #[serde(skip)]
    pub polygons: Vec<crate::storage::PolygonRings>,
    /// Further OSM ids folded into this place (merged street segments),
    /// findable through lookup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_ids: Vec<i64>,
}

impl Place {
    pub fn localized_name(&self, lang: Option<&str>) -> Option<&str> {
        lang.and_then(|l| self.names.get(l))
            .or(self.name.as_ref())
            .map(String::as_str)
    }

    /// Every name this place should be found by.
    pub fn all_names(&self) -> impl Iterator<Item = &str> {
        self.name
            .iter()
            .chain(self.names.values())
            .chain(self.alt_names.iter())
            .map(String::as_str)
    }

    /// Bounding box of the full geometry (the centre point for nodes).
    pub fn bbox(&self) -> BBox {
        let mut b = BBox::point(self.center);
        for line in &self.lines {
            for p in line {
                b.extend(*p);
            }
        }
        b
    }
}
