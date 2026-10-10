//! OSM tag filtering and classification into geocoder layers.

use std::collections::BTreeMap;

use geors_core::Layer;

/// Tags retained from an OSM element (only the keys we care about).
#[derive(Debug, Clone, Default)]
pub struct Tags(Vec<(String, String)>);

/// Keys that turn a named element into a POI, in priority order.
const POI_KEYS: &[&str] = &[
    "amenity",
    "shop",
    "tourism",
    "leisure",
    "historic",
    "office",
    "craft",
    "railway",
    "aeroway",
    "public_transport",
    "emergency",
    "healthcare",
    "man_made",
    "natural",
    "waterway",
    "sport",
    "club",
    "mountain_pass",
    "landuse",
    "building",
];

const ALT_NAME_KEYS: &[&str] = &[
    "alt_name",
    "old_name",
    "short_name",
    "official_name",
    "int_name",
    "loc_name",
    "reg_name",
];

const KEEP_KEYS: &[&str] = &[
    "place",
    "boundary",
    "admin_level",
    "type",
    "highway",
    "population",
    "wikidata",
    "wikipedia",
    "ISO3166-1",
    "ISO3166-1:alpha2",
    "ISO3166-2",
    "country_code",
    "area",
    "postal_code",
];

/// Highway values that represent addressable streets (ways only).
const STREET_VALUES: &[&str] = &[
    "motorway",
    "trunk",
    "primary",
    "secondary",
    "tertiary",
    "unclassified",
    "residential",
    "living_street",
    "pedestrian",
    "service",
    "road",
    "track",
    "footway",
    "path",
    "cycleway",
    "steps",
    "bridleway",
    "motorway_link",
    "trunk_link",
    "primary_link",
    "secondary_link",
    "tertiary_link",
    "busway",
];

/// Values that are never worth a search result even when named.
const IGNORED: &[(&str, &str)] = &[
    ("amenity", "parking_space"),
    ("amenity", "bench"),
    ("amenity", "waste_basket"),
    ("amenity", "vending_machine"),
    ("landuse", "residential"),
    ("landuse", "farmland"),
    ("landuse", "grass"),
    ("landuse", "meadow"),
    ("natural", "tree"),
    ("natural", "wood"),
    ("natural", "scrub"),
    ("railway", "rail"),
    ("railway", "abandoned"),
    ("railway", "disused"),
    ("power", "line"),
];

pub fn is_relevant_key(k: &str) -> bool {
    k.starts_with("name")
        || k.starts_with("addr:")
        || KEEP_KEYS.contains(&k)
        || POI_KEYS.contains(&k)
        || ALT_NAME_KEYS.contains(&k)
}

/// Cheap pre-check on raw tags before allocating anything.
pub fn may_be_interesting<'a>(mut tags: impl Iterator<Item = (&'a str, &'a str)>) -> bool {
    tags.any(|(k, _)| {
        k == "name"
            || k == "addr:housenumber"
            || k == "addr:housename"
            || k == "place"
            || k == "boundary"
    })
}

impl Tags {
    pub fn from_osm<'a>(it: impl Iterator<Item = (&'a str, &'a str)>) -> Self {
        Tags(
            it.filter(|(k, _)| is_relevant_key(k))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn name(&self) -> Option<&str> {
        self.get("name").map(str::trim).filter(|n| !n.is_empty())
    }

    /// `name:<lang>` values. Ignores compound keys like `name:etymology:wikidata`.
    pub fn localized_names(&self) -> BTreeMap<String, String> {
        self.0
            .iter()
            .filter_map(|(k, v)| {
                let lang = k.strip_prefix("name:")?;
                let ok = !lang.is_empty()
                    && lang.len() <= 10
                    && lang
                        .chars()
                        .all(|c| c.is_ascii_alphabetic() || c == '-' || c == '_');
                (ok && !v.trim().is_empty()).then(|| (lang.to_string(), v.trim().to_string()))
            })
            .collect()
    }

    pub fn alt_names(&self) -> Vec<String> {
        let mut out: Vec<String> = ALT_NAME_KEYS
            .iter()
            .filter_map(|k| self.get(k))
            .flat_map(|v| v.split(';'))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn admin_level(&self) -> Option<u8> {
        self.get("admin_level")?.trim().parse().ok()
    }

    pub fn country_code(&self) -> Option<String> {
        ["ISO3166-1:alpha2", "ISO3166-1", "country_code"]
            .iter()
            .filter_map(|k| self.get(k))
            .find_map(geors_core::normalize_country_code)
    }

    /// Country of a subdivision boundary, from its `ISO3166-2` code
    /// (`RU-MOS` -> `ru`).
    pub fn subdivision_country(&self) -> Option<String> {
        let code = self.get("ISO3166-2")?;
        let (cc, rest) = code.split_once('-')?;
        if rest.is_empty() {
            return None;
        }
        geors_core::normalize_country_code(cc)
    }

    pub fn population(&self) -> Option<u64> {
        let raw = self.get("population")?;
        raw.chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementKind {
    Node,
    Way,
    Relation,
}

/// What a tagged element is, for the geocoder.
#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub key: String,
    pub value: String,
    pub layer: Layer,
}

fn class(key: &str, value: &str, layer: Layer) -> Option<Class> {
    Some(Class {
        key: key.to_string(),
        value: value.to_string(),
        layer,
    })
}

pub fn place_layer(value: &str) -> Option<Layer> {
    Some(match value {
        "country" => Layer::Country,
        "state" | "province" | "region" => Layer::State,
        "county" => Layer::County,
        "city" | "town" | "village" | "hamlet" | "municipality" => Layer::City,
        "borough" | "suburb" | "quarter" | "neighbourhood" | "city_block" => Layer::District,
        "locality" | "isolated_dwelling" | "farm" | "square" | "island" | "islet" => {
            Layer::Locality
        }
        _ => return None,
    })
}

pub fn admin_level_layer(level: u8) -> Option<Layer> {
    Some(match level {
        2 => Layer::Country,
        3 | 4 => Layer::State,
        5 | 6 => Layer::County,
        7 | 8 => Layer::City,
        9 | 10 => Layer::District,
        _ => return None,
    })
}

/// Classify an element. Returns `None` if it should not be searchable.
pub fn classify(t: &Tags, kind: ElementKind) -> Option<Class> {
    let named = t.name().is_some();
    if named {
        if let Some(v) = t.get("place")
            && let Some(layer) = place_layer(v)
        {
            return class("place", v, layer);
        }
        if t.get("boundary") == Some("administrative")
            && kind == ElementKind::Relation
            && let Some(layer) = t.admin_level().and_then(admin_level_layer)
        {
            return class("boundary", "administrative", layer);
        }
        if let Some(v) = t.get("highway") {
            if kind == ElementKind::Way && STREET_VALUES.contains(&v) {
                return class("highway", v, Layer::Street);
            }
            if kind == ElementKind::Node
                && matches!(
                    v,
                    "bus_stop" | "motorway_junction" | "rest_area" | "services"
                )
            {
                return class("highway", v, Layer::Poi);
            }
        }
        for key in POI_KEYS {
            if let Some(v) = t.get(key) {
                if v == "no" || IGNORED.contains(&(*key, v)) {
                    continue;
                }
                // Linear waterways only when they are real rivers/canals.
                if *key == "waterway"
                    && !matches!(v, "river" | "canal" | "stream" | "waterfall" | "dam")
                {
                    continue;
                }
                return class(key, v, Layer::Poi);
            }
        }
    }
    if t.has("addr:housenumber") || t.has("addr:housename") {
        return match t.get("building") {
            Some(v) if v != "yes" && v != "no" => class("building", v, Layer::House),
            _ => class("place", "house", Layer::House),
        };
    }
    None
}

/// Importance prior in `[0, 1]`.
pub fn importance(c: &Class, t: &Tags) -> f32 {
    let mut imp = c.layer.base_importance();
    match (c.layer, c.value.as_str()) {
        (Layer::City, "city") => imp = 0.70,
        (Layer::City, "town") => imp = 0.62,
        (Layer::City, "village") => imp = 0.52,
        (Layer::City, "hamlet") => imp = 0.42,
        (Layer::Street, "motorway" | "trunk" | "primary") => imp += 0.05,
        (Layer::Street, "secondary" | "tertiary") => imp += 0.03,
        (
            Layer::Street,
            "service" | "track" | "footway" | "path" | "cycleway" | "steps" | "bridleway",
        ) => imp -= 0.08,
        _ => {}
    }
    if t.has("wikidata") || t.has("wikipedia") {
        imp += 0.1;
    }
    if let Some(pop) = t.population().filter(|&p| p > 0) {
        imp += 0.15 * ((pop as f32).log10() / 7.0).min(1.0);
    }
    imp.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> Tags {
        Tags::from_osm(pairs.iter().copied())
    }

    #[test]
    fn subdivision_country() {
        let c = |v| tags(&[("ISO3166-2", v)]).subdivision_country();
        assert_eq!(c("RU-MOS").as_deref(), Some("ru"));
        assert_eq!(c("DE-HB").as_deref(), Some("de"));
        assert_eq!(c("LI-01").as_deref(), Some("li"));
        assert_eq!(c("RU"), None);
        assert_eq!(c("RU-"), None);
        assert_eq!(c("XYZ-1"), None);
    }

    #[test]
    fn classification() {
        let c = |p: &[(&str, &str)], k| classify(&tags(p), k).map(|c| c.layer);
        assert_eq!(
            c(&[("place", "town"), ("name", "Vaduz")], ElementKind::Node),
            Some(Layer::City)
        );
        assert_eq!(
            c(
                &[("highway", "residential"), ("name", "Städtle")],
                ElementKind::Way
            ),
            Some(Layer::Street)
        );
        assert_eq!(c(&[("highway", "residential")], ElementKind::Way), None);
        assert_eq!(
            c(&[("amenity", "cafe"), ("name", "Café")], ElementKind::Node),
            Some(Layer::Poi)
        );
        assert_eq!(
            c(&[("amenity", "bench"), ("name", "x")], ElementKind::Node),
            None
        );
        assert_eq!(
            c(&[("addr:housenumber", "5")], ElementKind::Way),
            Some(Layer::House)
        );
        assert_eq!(
            c(
                &[
                    ("boundary", "administrative"),
                    ("admin_level", "2"),
                    ("name", "LI")
                ],
                ElementKind::Relation
            ),
            Some(Layer::Country)
        );
    }

    #[test]
    fn names() {
        let t = tags(&[
            ("name", "Vaduz"),
            ("name:de", "Vaduz"),
            ("name:etymology:wikidata", "Q1"),
            ("alt_name", "Faduz;  Vadutz"),
            ("ISO3166-1", "LI"),
        ]);
        assert_eq!(t.localized_names().len(), 1);
        assert_eq!(t.alt_names(), vec!["Faduz", "Vadutz"]);
        assert_eq!(t.country_code(), Some("li".into()));
    }
}
