//! Import of Nominatim dump files (the JSON format Photon imports from):
//! <https://github.com/komoot/photon/blob/master/docs/json-dump-format-0.1.0.md>
//!
//! A dump is a stream of `{"type": ..., "content": ...}` objects: one
//! `NominatimDumpFile` header, optional `CountryInfo`, then `Place` arrays.
//! Plain, `.jsonl` and gzip-compressed files are accepted.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use geors_core::geom::{BBox, LonLat};
use geors_core::storage::PolygonRings;
use geors_core::{AdminUnit, Layer, OsmType, Place};
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use crate::sink::CountrySink;
use crate::{Import, ImportOptions};

const ALT_NAME_KEYS: &[&str] = &[
    "alt_name",
    "old_name",
    "short_name",
    "official_name",
    "int_name",
    "loc_name",
    "reg_name",
];

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Value,
}

#[derive(Deserialize)]
struct Header {
    version: String,
    #[serde(default)]
    generator: Option<String>,
    #[serde(default)]
    data_timestamp: Option<String>,
}

#[derive(Deserialize)]
struct CountryInfo {
    country_code: String,
    #[serde(default)]
    name: BTreeMap<String, StrOrList>,
}

/// Name and address values may be a string or a list (first one displayed).
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum StrOrList {
    One(String),
    Many(Vec<String>),
}

impl StrOrList {
    fn all(&self) -> Vec<&str> {
        match self {
            StrOrList::One(s) => vec![s.as_str()],
            StrOrList::Many(v) => v.iter().map(String::as_str).collect(),
        }
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
    }

    fn first(&self) -> Option<&str> {
        self.all().into_iter().next()
    }
}

#[derive(Deserialize)]
struct AddressLine {
    place_id: Value,
    #[serde(default)]
    isaddress: bool,
}

#[derive(Deserialize)]
struct DumpPlace {
    #[serde(default)]
    place_id: Value,
    object_type: Option<String>,
    object_id: Option<i64>,
    osm_key: Option<String>,
    osm_value: Option<String>,
    address_type: Option<String>,
    rank_address: Option<i32>,
    importance: Option<f64>,
    #[serde(default)]
    name: BTreeMap<String, StrOrList>,
    housenumber: Option<String>,
    #[serde(default)]
    address: BTreeMap<String, StrOrList>,
    postcode: Option<String>,
    country_code: Option<String>,
    centroid: [f64; 2],
    bbox: Option<[f64; 4]>,
    geometry: Option<Value>,
    #[serde(default)]
    addresslines: Vec<AddressLine>,
}

fn layer_from_type(t: &str) -> Option<Layer> {
    Some(match t {
        "country" => Layer::Country,
        "state" => Layer::State,
        "county" => Layer::County,
        "city" => Layer::City,
        "district" => Layer::District,
        "locality" => Layer::Locality,
        "street" => Layer::Street,
        "house" => Layer::House,
        "other" => Layer::Poi,
        _ => return None,
    })
}

/// Nominatim address ranks, mapped the same way Photon does.
fn layer_from_rank(rank: i32) -> Layer {
    match rank {
        4 => Layer::Country,
        5..=9 => Layer::State,
        10..=12 => Layer::County,
        13..=16 => Layer::City,
        17..=21 => Layer::District,
        22..=25 => Layer::Locality,
        26..=27 => Layer::Street,
        28..=30 => Layer::House,
        _ => Layer::Poi,
    }
}

fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn coord(v: &Value) -> Option<LonLat> {
    let a = v.as_array()?;
    let p = LonLat::new(a.first()?.as_f64()?, a.get(1)?.as_f64()?);
    p.is_valid().then_some(p)
}

fn coords(v: &Value) -> Vec<LonLat> {
    v.as_array()
        .map(|a| a.iter().filter_map(coord).collect())
        .unwrap_or_default()
}

fn rings(v: &Value) -> PolygonRings {
    v.as_array()
        .map(|a| a.iter().map(coords).filter(|r| r.len() >= 4).collect())
        .unwrap_or_default()
}

/// GeoJSON geometry -> (lines, polygons).
fn geometry(g: &Value) -> (Vec<Vec<LonLat>>, Vec<PolygonRings>) {
    let c = &g["coordinates"];
    match g["type"].as_str() {
        Some("LineString") => (vec![coords(c)], vec![]),
        Some("MultiLineString") => (
            c.as_array()
                .map(|a| a.iter().map(coords).collect())
                .unwrap_or_default(),
            vec![],
        ),
        Some("Polygon") => (vec![], vec![rings(c)]),
        Some("MultiPolygon") => (
            vec![],
            c.as_array()
                .map(|a| a.iter().map(rings).collect())
                .unwrap_or_default(),
        ),
        _ => (vec![], vec![]),
    }
}

/// Split a name / address map into (default, localised) values.
fn split_names(
    map: &BTreeMap<String, StrOrList>,
    base: &str,
) -> (Option<String>, BTreeMap<String, String>) {
    let mut default = None;
    let mut localized = BTreeMap::new();
    for (k, v) in map {
        let Some(first) = v.first() else { continue };
        if k == base {
            default = Some(first.to_string());
        } else if let Some(lang) = k.strip_prefix(base).and_then(|r| r.strip_prefix(':')) {
            localized.insert(lang.to_string(), first.to_string());
        }
    }
    (default, localized)
}

/// What a previously seen place contributes when referenced in `addresslines`.
struct AddrRef {
    layer: Layer,
    unit: Option<u32>,
    name: String,
}

struct DumpBuilder {
    units: Vec<AdminUnit>,
    unit_ids: HashMap<(Layer, String, String), u32>,
    countries: HashMap<String, BTreeMap<String, String>>,
    refs: HashMap<String, AddrRef>,
    sink: CountrySink,
    count: u64,
    skipped: usize,
}

impl DumpBuilder {
    fn unit(
        &mut self,
        layer: Layer,
        name: String,
        names: BTreeMap<String, String>,
        cc: &str,
    ) -> u32 {
        let key = (layer, name.to_lowercase(), cc.to_string());
        if let Some(&u) = self.unit_ids.get(&key) {
            return u;
        }
        self.units.push(AdminUnit { layer, name, names });
        let u = (self.units.len() - 1) as u32;
        self.unit_ids.insert(key, u);
        u
    }

    fn country_unit(&mut self, cc: &str) -> u32 {
        let names = self.countries.get(cc).cloned().unwrap_or_default();
        let name = names
            .get("")
            .or_else(|| names.get("en"))
            .cloned()
            .unwrap_or_else(|| cc.to_ascii_uppercase());
        let localized = names.into_iter().filter(|(k, _)| !k.is_empty()).collect();
        self.unit(Layer::Country, name, localized, cc)
    }

    fn add(&mut self, d: DumpPlace) -> std::io::Result<()> {
        let center = LonLat::new(d.centroid[0], d.centroid[1]);
        if !center.is_valid() {
            self.skipped += 1;
            return Ok(());
        }
        let osm_type = match d.object_type.as_deref() {
            Some("N") => OsmType::Node,
            Some("W") => OsmType::Way,
            Some("R") => OsmType::Relation,
            _ => {
                self.skipped += 1;
                return Ok(());
            }
        };
        let cc = d
            .country_code
            .as_deref()
            .and_then(geors_core::normalize_country_code)
            .unwrap_or_default();

        let (name, names) = split_names(&d.name, "name");
        let name = name.or_else(|| names.values().next().cloned());
        let alt_names: Vec<String> = ALT_NAME_KEYS
            .iter()
            .filter_map(|k| d.name.get(*k))
            .flat_map(|v| v.all().into_iter().map(str::to_string))
            .collect();

        let mut layer = d
            .address_type
            .as_deref()
            .and_then(layer_from_type)
            .or_else(|| d.rank_address.map(layer_from_rank))
            .unwrap_or(if d.housenumber.is_some() {
                Layer::House
            } else {
                Layer::Poi
            });
        // Named things at address rank are POIs (shops with an address).
        if layer == Layer::House && name.is_some() {
            layer = Layer::Poi;
        }

        // Address: explicit `address` entries first, then `addresslines`.
        let mut parents: BTreeMap<Layer, u32> = BTreeMap::new();
        let mut street = None;
        let mut city_fallback = None;
        for kind in [
            "country", "state", "county", "city", "district", "locality", "street",
        ] {
            let (Some(value), localized) = split_names(&d.address, kind) else {
                continue;
            };
            match kind {
                "street" => street = Some(value),
                "country" if !cc.is_empty() => {
                    let u = self.unit(Layer::Country, value, localized, &cc);
                    parents.insert(Layer::Country, u);
                }
                _ => {
                    let l = layer_from_type(kind).unwrap();
                    // There is no locality slot in the address: use it as district.
                    let l = if l == Layer::Locality {
                        Layer::District
                    } else {
                        l
                    };
                    if l > layer && !parents.contains_key(&l) {
                        if l == Layer::City {
                            city_fallback = Some(value.clone());
                        }
                        let u = self.unit(l, value, localized, &cc);
                        parents.insert(l, u);
                    }
                }
            }
        }
        for line in d.addresslines.iter().filter(|l| l.isaddress) {
            let Some(r) = id_string(&line.place_id).and_then(|id| self.refs.get(&id)) else {
                continue;
            };
            match (r.layer, r.unit) {
                (Layer::Street, _) if street.is_none() => street = Some(r.name.clone()),
                (l, Some(u)) if l > layer => {
                    parents.entry(l).or_insert(u);
                }
                _ => {}
            }
        }
        if !cc.is_empty() && layer != Layer::Country && !parents.contains_key(&Layer::Country) {
            let u = self.country_unit(&cc);
            parents.insert(Layer::Country, u);
        }

        // Remember this place for later `addresslines` references.
        if let (Some(id), Some(n)) = (id_string(&d.place_id), &name)
            && (layer.is_admin() || layer == Layer::Street)
        {
            let unit = layer
                .is_admin()
                .then(|| self.unit(layer, n.clone(), names.clone(), &cc));
            self.refs.insert(
                id,
                AddrRef {
                    layer,
                    unit,
                    name: n.clone(),
                },
            );
        }

        let (lines, polygons) = d.geometry.as_ref().map(geometry).unwrap_or_default();
        let extent = d
            .bbox
            .map(|b| BBox::new(b[0], b[1], b[2], b[3]))
            .filter(|b| !b.is_empty() && (b.min_lon < b.max_lon || b.min_lat < b.max_lat));
        let base = layer.base_importance();
        self.count += 1;
        self.sink.push(Place {
            osm_type,
            osm_id: d.object_id.unwrap_or_default(),
            osm_key: d.osm_key.unwrap_or_else(|| "place".into()),
            osm_value: d.osm_value.unwrap_or_else(|| "yes".into()),
            layer,
            name,
            names,
            alt_names,
            housenumber: d.housenumber,
            street,
            postcode: d.postcode,
            city: city_fallback.filter(|_| !parents.contains_key(&Layer::City)),
            parents: parents.into_values().collect(),
            country_code: (!cc.is_empty()).then_some(cc),
            center,
            extent,
            importance: (d.importance.unwrap_or(0.0) as f32)
                .clamp(0.0, 1.0)
                .max(base),
            lines: lines.into_iter().filter(|l| l.len() >= 2).collect(),
            polygons,
            merged_ids: Vec::new(),
        })
    }
}

/// Import a dump file (optionally gzip-compressed).
pub fn import_dump(path: &Path, opts: &ImportOptions) -> Result<Import> {
    let file =
        File::open(path).with_context(|| format!("cannot open dump file '{}'", path.display()))?;
    let reader: Box<dyn Read> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(flate2::read::MultiGzDecoder::new(file))
    } else {
        Box::new(file)
    };
    import_dump_reader(BufReader::with_capacity(1 << 20, reader), opts)
        .with_context(|| format!("failed reading dump '{}'", path.display()))
}

pub fn import_dump_reader(reader: impl Read, opts: &ImportOptions) -> Result<Import> {
    let work = crate::work_dir(opts)?;
    let mut b = DumpBuilder {
        units: Vec::new(),
        unit_ids: HashMap::new(),
        countries: HashMap::new(),
        refs: HashMap::new(),
        sink: CountrySink::new(work.path.clone(), opts.default_country.clone()),
        count: 0,
        skipped: 0,
    };
    let mut stream = serde_json::Deserializer::from_reader(reader).into_iter::<Envelope>();
    let Some(first) = stream.next() else {
        bail!("dump file is empty")
    };
    let first = first.context("invalid JSON in dump header")?;
    if first.kind != "NominatimDumpFile" {
        bail!(
            "not a Nominatim dump file: first object has type '{}'",
            first.kind
        );
    }
    let header: Header = serde_json::from_value(first.content).context("invalid dump header")?;
    if !header.version.starts_with("0.1.") {
        warn!(version = %header.version, "unsupported dump version, expected 0.1.x; trying anyway");
    }
    info!(
        version = %header.version,
        generator = header.generator.as_deref().unwrap_or("-"),
        data_timestamp = header.data_timestamp.as_deref().unwrap_or("-"),
        "reading Nominatim dump"
    );
    for (n, obj) in stream.enumerate() {
        let obj = obj.with_context(|| format!("invalid JSON in object {}", n + 2))?;
        match obj.kind.as_str() {
            "CountryInfo" => {
                let infos: Vec<CountryInfo> =
                    serde_json::from_value(obj.content).context("invalid CountryInfo")?;
                b.countries.clear();
                for c in infos {
                    let Some(cc) = geors_core::normalize_country_code(&c.country_code) else {
                        continue;
                    };
                    let (default, mut localized) = split_names(&c.name, "name");
                    if let Some(d) = default {
                        localized.insert(String::new(), d);
                    }
                    b.countries.insert(cc, localized);
                }
            }
            "Place" => {
                // `content` is an array; tolerate a single object too.
                let items = match obj.content {
                    Value::Array(a) => a,
                    other => vec![other],
                };
                for item in items {
                    match serde_json::from_value::<DumpPlace>(item) {
                        Ok(p) => b.add(p)?,
                        Err(e) => {
                            b.skipped += 1;
                            if b.skipped <= 5 {
                                warn!(error = %e, "skipping invalid place");
                            }
                        }
                    }
                }
            }
            _ => {} // unknown types must be ignored
        }
    }
    if b.skipped > 0 {
        warn!(
            skipped = b.skipped,
            "places skipped (no OSM id type, bad centroid or invalid JSON)"
        );
    }
    info!(places = b.count, admin_units = b.units.len(), "dump read");

    let country_names: HashMap<String, String> = b
        .countries
        .iter()
        .filter_map(|(cc, names)| Some((cc.clone(), names.get("")?.clone())))
        .collect();
    let countries = b.sink.finish(opts, b.units, &country_names)?;
    Ok(Import {
        countries,
        _work: work,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"
{"type":"NominatimDumpFile","content":{"version":"0.1.0","generator":"test","data_timestamp":"2026-10-01T00:00:00Z"}}
{"type":"CountryInfo","content":[{"country_code":"li","name":{"name":"Liechtenstein","name:ru":"Лихтенштейн"}}]}
{"type":"Place","content":[{"place_id":1,"object_type":"N","object_id":10,"osm_key":"place","osm_value":"town","rank_address":16,"importance":0.4,"name":{"name":"Vaduz","name:ru":"Вадуц"},"country_code":"li","centroid":[9.52,47.14]}]}
{"type":"Place","content":[{"place_id":2,"object_type":"W","object_id":20,"osm_key":"highway","osm_value":"pedestrian","address_type":"street","name":{"name":"Städtle"},"addresslines":[{"place_id":1,"isaddress":true}],"country_code":"li","centroid":[9.522,47.139],"geometry":{"type":"LineString","coordinates":[[9.521,47.139],[9.523,47.140]]}}]}
{"type":"Place","content":[{"place_id":3,"object_type":"W","object_id":30,"osm_key":"building","osm_value":"yes","address_type":"house","housenumber":"35","address":{"street":"Städtle","city":"Vaduz"},"postcode":"9490","country_code":"LI","centroid":[9.5221,47.1395],"bbox":[9.522,47.139,9.5222,47.1396]},{"place_id":3,"object_type":"W","object_id":30,"address_type":"house","housenumber":"35a","country_code":"li","centroid":[9.5221,47.1395]}]}
{"type":"photon:custom","content":{}}
{"type":"Place","content":[{"place_id":4,"object_type":"X","object_id":1,"centroid":[1,1]}]}
"#;

    #[test]
    fn reads_dump() {
        let import = import_dump_reader(DUMP.as_bytes(), &ImportOptions::default()).unwrap();
        assert_eq!(import.countries.len(), 1);
        let li = &import.countries[0];
        assert_eq!(li.country_code, "li");
        assert_eq!(li.country_name.as_deref(), Some("Liechtenstein"));
        assert_eq!(li.places.count, 4);
        let reader = geors_core::spill::SpillReader::open(&li.places).unwrap();
        let places: Vec<Place> = reader
            .index()
            .unwrap()
            .into_iter()
            .map(|e| reader.get(e.offset).unwrap())
            .collect();

        let unit = |p: &Place, l: Layer| {
            p.parents
                .iter()
                .map(|&u| &li.admins[u as usize])
                .find(|a| a.layer == l)
                .map(|a| a.name.clone())
        };
        let town = &places[0];
        assert_eq!(town.layer, Layer::City);
        assert_eq!(town.names.get("ru").map(String::as_str), Some("Вадуц"));
        assert_eq!(unit(town, Layer::Country).as_deref(), Some("Liechtenstein"));

        let street = &places[1];
        assert_eq!(street.layer, Layer::Street);
        assert_eq!(street.lines.len(), 1);
        // City comes from the addressline reference to place 1.
        assert_eq!(unit(street, Layer::City).as_deref(), Some("Vaduz"));

        let house = &places[2];
        assert_eq!(house.layer, Layer::House);
        assert_eq!(house.street.as_deref(), Some("Städtle"));
        assert_eq!(house.postcode.as_deref(), Some("9490"));
        assert_eq!(unit(house, Layer::City).as_deref(), Some("Vaduz"));
        assert!(house.extent.is_some());
        // Same `city` name from `address` and `addresslines` is one unit.
        let city_units: Vec<_> = li
            .admins
            .iter()
            .filter(|a| a.layer == Layer::City)
            .collect();
        assert_eq!(city_units.len(), 1);
    }

    #[test]
    fn rejects_non_dump() {
        let err = import_dump_reader(
            r#"{"type":"Place","content":[]}"#.as_bytes(),
            &ImportOptions::default(),
        );
        assert!(err.is_err());
    }
}
