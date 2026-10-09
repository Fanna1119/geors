//! OSM PBF ingestion: turns an extract into per-country lists of [`Place`]s
//! with an address hierarchy, ready to be indexed.
//!
//! Pipeline:
//! 1. [`reader::read_pbf`]: three streaming passes over the file, keeping
//!    only relevant elements and the node coordinates they need.
//! 2. Geometry: ways become lines (streets) or polygons; relations are
//!    assembled into multipolygons ([`rings`]).
//! 3. Hierarchy: admin boundaries (and `place=*` nodes as a fallback) give
//!    every place its district / city / county / state / country ([`areas`]).
//! 4. Street segments are merged ([`streets`]).
//! 5. Places are partitioned by country code.

pub mod areas;
pub mod reader;
pub mod rings;
pub mod streets;
pub mod tags;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::Result;
use geo::{BoundingRect, InteriorPoint, MultiPolygon, Simplify};
use geors_core::geom::{BBox, LonLat};
use geors_core::{AdminUnit, Layer, OsmType, Place};
use tracing::{info, warn};

use crate::areas::{Area, AreaIndex, LocalityIndex, LocalityPoint};
use crate::reader::RawData;
use crate::tags::{Class, Tags};

/// Douglas-Peucker tolerance for stored line geometry (~1 m).
const SIMPLIFY_DEG: f64 = 0.00001;

#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Only keep these countries (lowercase ISO codes). Empty = automatic.
    pub countries: Vec<String>,
    /// Keep every country found, even small slivers across the border.
    pub all_countries: bool,
    /// Country for places that are not inside any country boundary.
    pub default_country: Option<String>,
    /// In automatic mode, drop countries with less than this share of places.
    pub min_share: f64,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            countries: Vec::new(),
            all_countries: false,
            default_country: None,
            min_share: 0.01,
        }
    }
}

/// Everything needed to write one country partition.
pub struct CountryData {
    /// Lowercase ISO 3166-1 alpha-2 code.
    pub country_code: String,
    pub country_name: Option<String>,
    pub places: Vec<Place>,
    pub admins: Vec<AdminUnit>,
}

pub fn import_pbf(path: &Path, opts: &ImportOptions) -> Result<Vec<CountryData>> {
    let raw = reader::read_pbf(path)?;
    Ok(build(raw, opts))
}

fn make_place(osm_type: OsmType, osm_id: i64, t: &Tags, c: &Class, center: LonLat) -> Place {
    let name = t
        .name()
        .or_else(|| {
            (c.layer == Layer::House)
                .then(|| t.get("addr:housename"))
                .flatten()
        })
        .map(str::to_string);
    Place {
        osm_type,
        osm_id,
        osm_key: c.key.clone(),
        osm_value: c.value.clone(),
        layer: c.layer,
        name,
        names: t.localized_names(),
        alt_names: t.alt_names(),
        housenumber: t.get("addr:housenumber").map(str::to_string),
        street: t
            .get("addr:street")
            .or_else(|| t.get("addr:place"))
            .map(str::to_string),
        postcode: t.get("addr:postcode").map(str::to_string),
        city: t.get("addr:city").map(str::to_string),
        parents: Vec::new(),
        country_code: t
            .get("addr:country")
            .and_then(geors_core::normalize_country_code),
        center,
        extent: None,
        importance: tags::importance(c, t),
        lines: Vec::new(),
    }
}

fn bbox_of(mp: &MultiPolygon<f64>) -> Option<BBox> {
    mp.bounding_rect()
        .map(|r| BBox::new(r.min().x, r.min().y, r.max().x, r.max().y))
}

fn simplify(line: &[LonLat]) -> Vec<LonLat> {
    rings::to_linestring(line)
        .simplify(SIMPLIFY_DEG)
        .0
        .iter()
        .map(|c| LonLat::new(c.x, c.y))
        .collect()
}

/// Collects places and admin units while walking the raw data.
#[derive(Default)]
struct Builder {
    units: Vec<AdminUnit>,
    areas: Vec<Area>,
    localities: Vec<LocalityPoint>,
    places: Vec<Place>,
    /// For each place, the admin unit it itself represents (excluded from its parents).
    self_unit: Vec<Option<u32>>,
}

impl Builder {
    fn unit(&mut self, t: &Tags, layer: Layer) -> u32 {
        self.units.push(AdminUnit {
            layer,
            name: t.name().unwrap_or_default().to_string(),
            names: t.localized_names(),
        });
        (self.units.len() - 1) as u32
    }

    fn area(
        &mut self,
        t: &Tags,
        layer: Layer,
        rank: u8,
        polygon: &MultiPolygon<f64>,
    ) -> Option<u32> {
        let bbox = bbox_of(polygon)?;
        let unit = self.unit(t, layer);
        let country_code = if layer == Layer::Country {
            t.country_code()
        } else {
            None
        };
        self.areas.push(Area {
            unit,
            layer,
            rank,
            polygon: polygon.clone(),
            bbox,
            country_code,
        });
        Some(unit)
    }

    fn push(&mut self, place: Place, self_unit: Option<u32>) -> usize {
        self.places.push(place);
        self.self_unit.push(self_unit);
        self.places.len() - 1
    }
}

fn build(raw: RawData, opts: &ImportOptions) -> Vec<CountryData> {
    let mut b = Builder::default();
    let way_refs = |id: i64| -> Option<&[i64]> {
        raw.ways
            .binary_search_by_key(&id, |w| w.id)
            .ok()
            .map(|i| raw.ways[i].refs.as_slice())
    };

    // Nodes. Place nodes double as locality units for the hierarchy fallback.
    let mut place_nodes: HashMap<i64, usize> = HashMap::new();
    for n in &raw.nodes {
        let place = make_place(OsmType::Node, n.id, &n.tags, &n.class, n.point);
        let mut self_unit = None;
        if n.class.key == "place"
            && matches!(n.class.layer, Layer::City | Layer::District)
            && let Some(radius_m) = areas::place_radius_m(&n.class.value)
        {
            let unit = b.unit(&n.tags, n.class.layer);
            b.localities.push(LocalityPoint {
                unit,
                layer: n.class.layer,
                point: n.point,
                radius_m,
            });
            self_unit = Some(unit);
        }
        let i = b.push(place, self_unit);
        if n.class.key == "place" {
            place_nodes.insert(n.id, i);
        }
    }

    // Ways: streets and rivers are lines, closed ways are areas.
    for w in &raw.ways {
        let Some((t, c)) = &w.feature else { continue };
        let pts = raw.coords.line(&w.refs);
        if pts.is_empty() {
            continue;
        }
        let closed = w.refs.len() >= 4 && w.refs.first() == w.refs.last();
        let linear = !closed || c.layer == Layer::Street || c.key == "waterway";
        let mut self_unit = None;
        let place = if linear {
            let line = simplify(&pts);
            let center = streets::line_midpoint(&line).unwrap_or(pts[0]);
            let mut p = make_place(OsmType::Way, w.id, t, c, center);
            p.lines = vec![line];
            p
        } else {
            let polygon = geo::Polygon::new(rings::to_linestring(&pts), vec![]);
            let mp = MultiPolygon::new(vec![polygon]);
            let center = mp
                .interior_point()
                .map(|p| LonLat::new(p.x(), p.y()))
                .unwrap_or(pts[0]);
            if c.key == "place" && c.layer.is_admin() {
                self_unit = b.area(t, c.layer, 11, &mp);
            }
            let mut p = make_place(OsmType::Way, w.id, t, c, center);
            p.extent = bbox_of(&mp);
            p
        };
        b.push(place, self_unit);
    }

    // Relations: admin boundaries feed the hierarchy; named ones are places too.
    let mut broken = 0usize;
    for r in &raw.relations {
        let outer: Vec<&[i64]> = r.outer.iter().filter_map(|&id| way_refs(id)).collect();
        let inner: Vec<&[i64]> = r.inner.iter().filter_map(|&id| way_refs(id)).collect();
        let (mp, n_broken) = rings::assemble(outer, inner, &raw.coords);
        if mp.is_none() {
            broken += (n_broken > 0) as usize;
        }
        let admin = if r.is_admin_boundary() {
            r.tags
                .admin_level()
                .and_then(tags::admin_level_layer)
                .map(|l| (l, r.tags.admin_level().unwrap()))
        } else {
            r.class
                .as_ref()
                .filter(|c| c.key == "place" && c.layer.is_admin())
                .map(|c| (c.layer, 11))
        };
        let mut self_unit = None;
        if let (Some((layer, rank)), Some(mp)) = (admin, &mp) {
            self_unit = b.area(&r.tags, layer, rank, mp);
        }
        let Some(class) = &r.class else { continue };
        let extent = mp.as_ref().and_then(bbox_of);

        // A boundary whose label / admin_centre is the matching place node is
        // the same real-world place: keep the node, give it the extent.
        let name = r.tags.name();
        let twin = r
            .label
            .and_then(|id| place_nodes.get(&id))
            .filter(|&&i| b.places[i].layer == class.layer)
            .or_else(|| {
                r.admin_centre
                    .and_then(|id| place_nodes.get(&id))
                    .filter(|&&i| {
                        b.places[i].layer == class.layer && b.places[i].name.as_deref() == name
                    })
            })
            .copied();
        if let Some(i) = twin {
            if b.places[i].extent.is_none() {
                b.places[i].extent = extent;
            }
            continue;
        }
        let center = r
            .label
            .or(r.admin_centre)
            .and_then(|id| raw.coords.get(id))
            .or_else(|| {
                mp.as_ref()?
                    .interior_point()
                    .map(|p| LonLat::new(p.x(), p.y()))
            });
        let Some(center) = center else { continue };
        let mut place = make_place(OsmType::Relation, r.id, &r.tags, class, center);
        place.extent = extent;
        b.push(place, self_unit);
    }
    if broken > 0 {
        warn!(
            broken,
            "relations with incomplete geometry (probably clipped by the extract); they are not used for the hierarchy"
        );
    }
    info!(
        places = b.places.len(),
        admin_areas = b.areas.len(),
        localities = b.localities.len(),
        "features extracted"
    );

    // Hierarchy.
    let Builder {
        units,
        areas,
        localities,
        mut places,
        self_unit,
    } = b;
    let mut area_index = AreaIndex::new(areas);
    let locality_index = LocalityIndex::new(localities);
    let mut country_names: HashMap<String, String> = HashMap::new();
    for a in &area_index.areas {
        if let Some(cc) = &a.country_code {
            country_names
                .entry(cc.clone())
                .or_insert_with(|| units[a.unit as usize].name.clone());
        }
    }
    for (place, own) in places.iter_mut().zip(&self_unit) {
        let mut best: BTreeMap<Layer, (u8, u32)> = BTreeMap::new();
        let mut country = None;
        for ai in area_index.containing(place.center) {
            let a = &area_index.areas[ai];
            if a.layer == Layer::Country && country.is_none() {
                country = a.country_code.clone();
            }
            if a.layer <= place.layer || Some(a.unit) == *own {
                continue;
            }
            let e = best.entry(a.layer).or_insert((a.rank, a.unit));
            if a.rank > e.0 {
                *e = (a.rank, a.unit);
            }
        }
        for layer in [Layer::City, Layer::District] {
            if place.layer < layer
                && !best.contains_key(&layer)
                && let Some(u) = locality_index
                    .best(place.center, layer)
                    .filter(|u| Some(*u) != *own)
            {
                best.insert(layer, (0, u));
            }
        }
        place.parents = best.values().map(|(_, u)| *u).collect();
        place.country_code = country.or(place.country_code.take());
    }

    let places = streets::merge(places, |p| {
        p.parents
            .iter()
            .copied()
            .find(|&u| units[u as usize].layer == Layer::City)
    });

    partition(places, &units, &country_names, opts)
}

fn partition(
    places: Vec<Place>,
    units: &[AdminUnit],
    country_names: &HashMap<String, String>,
    opts: &ImportOptions,
) -> Vec<CountryData> {
    let mut by_cc: BTreeMap<String, Vec<Place>> = BTreeMap::new();
    let mut unknown = 0usize;
    for mut p in places {
        match p
            .country_code
            .clone()
            .or_else(|| opts.default_country.clone())
        {
            Some(cc) => {
                p.country_code = Some(cc.to_ascii_uppercase());
                by_cc.entry(cc).or_default().push(p);
            }
            None => unknown += 1,
        }
    }
    if unknown > 0 {
        warn!(
            unknown,
            "places outside any country boundary were skipped (use --default-country to keep them)"
        );
    }
    let total: usize = by_cc.values().map(Vec::len).sum();
    let found: Vec<String> = by_cc
        .iter()
        .map(|(cc, v)| format!("{cc}={}", v.len()))
        .collect();
    info!(countries = %found.join(" "), "places per country");

    if !opts.countries.is_empty() {
        for cc in &opts.countries {
            if !by_cc.contains_key(cc) {
                warn!(country = %cc, "requested country has no places in this extract");
            }
        }
        by_cc.retain(|cc, _| opts.countries.contains(cc));
    } else if !opts.all_countries {
        by_cc.retain(|cc, v| {
            let keep = v.len() as f64 >= opts.min_share * total as f64;
            if !keep {
                info!(country = %cc, places = v.len(), "skipping border sliver (use --all-countries or --countries to keep it)");
            }
            keep
        });
    }

    by_cc
        .into_iter()
        .map(|(cc, mut places)| {
            // Each partition gets its own compact admin table.
            let mut remap: HashMap<u32, u32> = HashMap::new();
            let mut admins = Vec::new();
            for p in &mut places {
                for u in &mut p.parents {
                    *u = *remap.entry(*u).or_insert_with(|| {
                        admins.push(units[*u as usize].clone());
                        (admins.len() - 1) as u32
                    });
                }
            }
            CountryData {
                country_name: country_names.get(&cc).cloned(),
                country_code: cc,
                places,
                admins,
            }
        })
        .collect()
}
