//! Ingestion: turns an OSM extract (or a Nominatim dump) into per-country
//! streams of [`Place`]s with an address hierarchy, ready to be indexed.
//!
//! The PBF pipeline is streaming, so memory does not grow with the number
//! of places:
//!
//! 1. Five passes over the file ([`reader`]); only relations, their member
//!    ways and `place=*` nodes are kept in memory. Coordinates of needed
//!    nodes go to a disk-backed store ([`nodes`]).
//! 2. Admin, postcode and `place=*` areas are assembled ([`rings`]) and
//!    indexed for point-in-polygon lookups ([`areas`]).
//! 3. Way and node features are built one at a time, get their hierarchy,
//!    and are spilled to disk per country ([`sink`]).
//! 4. Street segments are spilled separately and merged group by group
//!    ([`streets`]).
//!
//! Peak memory is roughly: relations + boundary polygons + 48 bytes per
//! street segment, plus the page cache for 16 bytes per needed node.

pub mod areas;
pub mod dump;
pub mod extsort;
pub mod join;
pub mod nodes;
pub mod pip;
pub mod reader;
pub mod rings;
pub mod sink;
pub mod streets;
pub mod tags;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use geo::{BoundingRect, InteriorPoint, MultiPolygon, Simplify};
use geors_core::geom::{BBox, LonLat};
use geors_core::spill::{Spill, WorkDir};
use geors_core::storage::PolygonRings;
use geors_core::{AdminUnit, Layer, OsmType, Place};
use rayon::prelude::*;
use tracing::{info, warn};

use crate::areas::{Area, AreaIndex, CellCache, LocalityIndex, LocalityPoint};
use crate::join::{RefSink, WayCursor};
use crate::nodes::{IdSink, NodeCoords};
use crate::pip::BandedPolygon;
use crate::sink::CountrySink;
use crate::streets::StreetStore;
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
    /// Where scratch files go (default: the system temp dir). Needs room for
    /// roughly the size of the resulting partitions.
    pub work_dir: Option<PathBuf>,
    /// How ways find their nodes' coordinates.
    pub node_lookup: NodeLookup,
}

/// How ways get their nodes' coordinates during import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeLookup {
    /// Sorted join when the coordinates would not fit in a quarter of the
    /// available memory, else memory.
    #[default]
    Auto = 0,
    /// Random lookups in a memory-mapped store: fastest when it fits in RAM.
    Memory = 1,
    /// Sequential sort-merge join ([`join`]): for extracts larger than RAM.
    Sorted = 2,
}

impl std::str::FromStr for NodeLookup {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "auto" => Ok(NodeLookup::Auto),
            "memory" => Ok(NodeLookup::Memory),
            "sorted" => Ok(NodeLookup::Sorted),
            _ => Err(format!("unknown node lookup '{s}' (auto, memory, sorted)")),
        }
    }
}

static NODE_LOOKUP: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Process-wide default for [`ImportOptions::node_lookup`] (CLI flag).
pub fn set_default_node_lookup(mode: NodeLookup) {
    NODE_LOOKUP.store(mode as u8, std::sync::atomic::Ordering::Relaxed);
}

fn default_node_lookup() -> NodeLookup {
    match NODE_LOOKUP.load(std::sync::atomic::Ordering::Relaxed) {
        1 => NodeLookup::Memory,
        2 => NodeLookup::Sorted,
        _ => NodeLookup::Auto,
    }
}

/// Memory available to this process: the container limit if there is one,
/// else physical RAM. `None` if unknown.
pub fn memory_limit() -> Option<u64> {
    let read = |p: &str| std::fs::read_to_string(p).ok();
    // cgroup v2, then v1 (v1 reports a huge number when unlimited).
    if let Some(v) = read("/sys/fs/cgroup/memory.max").and_then(|s| s.trim().parse::<u64>().ok()) {
        return Some(v);
    }
    if let Some(v) = read("/sys/fs/cgroup/memory/memory.limit_in_bytes")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v < 1 << 60)
    {
        return Some(v);
    }
    if let Some(kb) = read("/proc/meminfo").and_then(|m| {
        m.lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
    }) {
        return Some(kb * 1024);
    }
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Whether to use the sorted join for this extract.
fn use_sorted_join(path: &Path, mode: NodeLookup) -> bool {
    match mode {
        NodeLookup::Memory => false,
        NodeLookup::Sorted => true,
        NodeLookup::Auto => {
            // Measured: the coordinate store is about half the PBF size
            // (Germany 4.6 GB -> 2.4 GB, Europe 33 GB -> 13 GB).
            let pbf = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let store = pbf / 2;
            let sorted = memory_limit().is_some_and(|mem| store > mem / 4);
            info!(
                store_estimate_mb = store >> 20,
                memory_mb = memory_limit().map(|m| m >> 20),
                mode = if sorted { "sorted join" } else { "memory" },
                "node lookup"
            );
            sorted
        }
    }
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            countries: Vec::new(),
            all_countries: false,
            default_country: None,
            min_share: 0.01,
            work_dir: None,
            node_lookup: default_node_lookup(),
        }
    }
}

/// Everything needed to write one country partition.
pub struct CountryData {
    /// Lowercase ISO 3166-1 alpha-2 code.
    pub country_code: String,
    pub country_name: Option<String>,
    /// The country's places, spilled to disk.
    pub places: Spill,
    /// Admin table `Place::parents` refers to (shared across countries).
    pub admins: Arc<Vec<AdminUnit>>,
}

/// Result of an import. The spill files live in a scratch directory that is
/// deleted when this is dropped, so write the partitions first.
pub struct Import {
    pub countries: Vec<CountryData>,
    pub(crate) _work: WorkDir,
}

pub(crate) fn work_dir(opts: &ImportOptions) -> Result<WorkDir> {
    let parent = opts.work_dir.clone().unwrap_or_else(std::env::temp_dir);
    WorkDir::create(&parent, "geors-import")
        .with_context(|| format!("cannot create scratch directory in '{}'", parent.display()))
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
        polygons: Vec::new(),
        merged_ids: Vec::new(),
    }
}

fn bbox_of(mp: &MultiPolygon<f64>) -> Option<BBox> {
    mp.bounding_rect()
        .map(|r| BBox::new(r.min().x, r.min().y, r.max().x, r.max().y))
}

/// Polygon rings for output, simplified relative to the polygon's size
/// (~1 m for buildings, tens of metres for countries).
fn polygon_rings(mp: &MultiPolygon<f64>) -> Vec<PolygonRings> {
    let Some(b) = bbox_of(mp) else {
        return Vec::new();
    };
    let diag = ((b.max_lon - b.min_lon).powi(2) + (b.max_lat - b.min_lat).powi(2)).sqrt();
    let eps = (diag * 5e-4).clamp(SIMPLIFY_DEG, 0.01);
    let ring = |ls: &geo::LineString<f64>| -> Vec<LonLat> {
        ls.simplify(eps)
            .0
            .iter()
            .map(|c| LonLat::new(c.x, c.y))
            .collect()
    };
    mp.iter()
        .filter_map(|poly| {
            let exterior = ring(poly.exterior());
            if exterior.len() < 4 {
                return None;
            }
            let mut rings = vec![exterior];
            rings.extend(poly.interiors().iter().map(ring).filter(|r| r.len() >= 4));
            Some(rings)
        })
        .collect()
}

fn simplify(line: &[LonLat]) -> Vec<LonLat> {
    rings::to_linestring(line)
        .simplify(SIMPLIFY_DEG)
        .0
        .iter()
        .map(|c| LonLat::new(c.x, c.y))
        .collect()
}

/// Assigns parents (admin units), country and postcode to places.
struct Hierarchy {
    units: Vec<AdminUnit>,
    areas: AreaIndex,
    postcodes: AreaIndex,
    postcode_names: Vec<String>,
    localities: LocalityIndex,
}

/// Per-worker point-in-polygon caches.
#[derive(Default)]
struct HierCache {
    areas: CellCache,
    postcodes: CellCache,
}

impl Hierarchy {
    /// Read-only apart from the caller's cache, so workers run it in parallel.
    fn assign(&self, cache: &mut HierCache, place: &mut Place, own: Option<u32>) {
        let mut best: BTreeMap<Layer, (u8, u32)> = BTreeMap::new();
        let mut country = None;
        let mut region_country = None;
        for ai in self.areas.containing(&mut cache.areas, place.center) {
            let a = &self.areas.areas[ai];
            if let Some(cc) = &a.country_code {
                let slot = if a.layer == Layer::Country {
                    &mut country
                } else {
                    &mut region_country
                };
                if slot.is_none() {
                    *slot = Some(cc.clone());
                }
            }
            if a.layer <= place.layer || Some(a.unit) == own {
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
                && let Some(u) = self
                    .localities
                    .best(place.center, layer)
                    .filter(|u| Some(*u) != own)
            {
                best.insert(layer, (0, u));
            }
        }
        place.parents = best.values().map(|(_, u)| *u).collect();
        // A country boundary cut by the extract's edge is unusable, but the
        // regions inside it (states, provinces) usually are complete and
        // name their country in `ISO3166-2`.
        place.country_code = country.or(region_country).or(place.country_code.take());
        if place.postcode.is_none() && place.layer <= Layer::Street {
            place.postcode = self
                .postcodes
                .containing(&mut cache.postcodes, place.center)
                .first()
                .map(|&i| self.postcode_names[self.postcodes.areas[i].unit as usize].clone());
        }
    }

    fn city_of(&self, place: &Place) -> Option<u32> {
        place
            .parents
            .iter()
            .copied()
            .find(|&u| self.units[u as usize].layer == Layer::City)
    }
}

/// Routes finished places (on the consumer thread): streets to the street
/// store for merging, everything else to the country spills.
struct Router {
    sink: CountrySink,
    streets: StreetStore,
    count: u64,
    started: Instant,
}

impl Router {
    /// `city` is the street's city unit (from [`Hierarchy::city_of`]).
    fn route(&mut self, place: Place, city: Option<u32>) -> Result<()> {
        if streets::is_mergeable(&place) {
            self.streets.push(&place, city)?;
        } else {
            self.sink.push(place)?;
        }
        self.count += 1;
        if self.count.is_multiple_of(1_000_000) {
            info!(places = self.count, elapsed = ?self.started.elapsed(), "progress");
        }
        Ok(())
    }
}

fn unit(units: &mut Vec<AdminUnit>, t: &Tags, layer: Layer) -> u32 {
    units.push(AdminUnit {
        layer,
        name: t.name().unwrap_or_default().to_string(),
        names: t.localized_names(),
    });
    (units.len() - 1) as u32
}

fn add_area(
    units: &mut Vec<AdminUnit>,
    areas: &mut Vec<Area>,
    t: &Tags,
    layer: Layer,
    rank: u8,
    polygon: BandedPolygon,
    bbox: BBox,
) -> u32 {
    let unit = unit(units, t, layer);
    let country_code = if layer == Layer::Country {
        t.country_code()
    } else {
        t.subdivision_country()
    };
    areas.push(Area {
        unit,
        layer,
        rank,
        polygon,
        bbox,
        country_code,
    });
    unit
}

/// Everything derived from a relation's geometry, computed in parallel.
struct RelGeometry {
    /// Member ways could not be joined into closed rings.
    broken: bool,
    /// Admin (or `place=*`) area: layer, rank, polygon index, bbox.
    admin: Option<(Layer, u8, BandedPolygon, BBox)>,
    /// Postcode area: code, polygon index, bbox.
    postcode: Option<(String, BandedPolygon, BBox)>,
    /// Searchable feature: centre, extent, simplified outline.
    feature: Option<(Option<LonLat>, Option<BBox>, Vec<PolygonRings>)>,
}

fn relation_geometry(
    r: &reader::RelationData,
    member_refs: &HashMap<i64, Vec<i64>>,
    coords: &NodeCoords,
) -> RelGeometry {
    let refs = |ids: &[i64]| -> Vec<&[i64]> {
        ids.iter()
            .filter_map(|id| member_refs.get(id).map(Vec::as_slice))
            .collect()
    };
    let (mp, n_broken) = rings::assemble(refs(&r.outer), refs(&r.inner), coords);
    let bbox = mp.as_ref().and_then(bbox_of);
    let admin_kind = if r.is_admin_boundary() {
        r.tags
            .admin_level()
            .and_then(|l| tags::admin_level_layer(l).map(|layer| (layer, l)))
    } else {
        r.class
            .as_ref()
            .filter(|c| c.key == "place" && c.layer.is_admin())
            .map(|c| (c.layer, 11))
    };
    let admin = match (admin_kind, &mp, bbox) {
        (Some((layer, rank)), Some(mp), Some(b)) => Some((layer, rank, BandedPolygon::new(mp), b)),
        _ => None,
    };
    let postcode = match (r.postal_code(), &mp, bbox) {
        (Some(code), Some(mp), Some(b)) => Some((code.to_string(), BandedPolygon::new(mp), b)),
        _ => None,
    };
    let feature = r.class.as_ref().map(|_| {
        let center = r
            .label
            .or(r.admin_centre)
            .and_then(|id| coords.get(id))
            .or_else(|| mp.as_ref().and_then(interior));
        (
            center,
            bbox,
            mp.as_ref().map(polygon_rings).unwrap_or_default(),
        )
    });
    RelGeometry {
        broken: mp.is_none() && n_broken > 0,
        admin,
        postcode,
        feature,
    }
}

fn interior(mp: &MultiPolygon<f64>) -> Option<LonLat> {
    mp.interior_point().map(|p| LonLat::new(p.x(), p.y()))
}

/// A relation that is a searchable place. Emitted at the end unless a
/// matching `place=*` node (its label / admin centre) absorbs it.
struct RelFeature {
    rel: usize,
    class: Class,
    center: Option<LonLat>,
    extent: Option<BBox>,
    polygons: Vec<PolygonRings>,
    own: Option<u32>,
    absorbed: bool,
}

/// Build the place for a way feature (geometry from `coords`).
fn way_place(id: i64, refs: &[i64], pts: Vec<LonLat>, t: &Tags, c: &Class) -> Option<Place> {
    let first = *pts.first()?;
    let linear = !reader::is_closed(refs) || c.layer == Layer::Street || c.key == "waterway";
    if linear {
        let line = simplify(&pts);
        let center = streets::line_midpoint(&line).unwrap_or(first);
        let mut p = make_place(OsmType::Way, id, t, c, center);
        p.lines = vec![line];
        Some(p)
    } else {
        let polygon = geo::Polygon::new(rings::to_linestring(&pts), vec![]);
        let mp = MultiPolygon::new(vec![polygon]);
        let mut p = make_place(OsmType::Way, id, t, c, interior(&mp).unwrap_or(first));
        p.extent = bbox_of(&mp);
        p.polygons = polygon_rings(&mp);
        Some(p)
    }
}

pub fn import_pbf(path: &Path, opts: &ImportOptions) -> Result<Import> {
    let started = Instant::now();
    let work = work_dir(opts)?;

    // Pass 1-2: what is needed.
    let (index, relations) = reader::read_relations(path)?;
    let member_ways: HashSet<i64> = relations
        .iter()
        .flat_map(|r| r.outer.iter().chain(&r.inner).copied())
        .collect();
    let mut ids = IdSink::create(&work.path.join("node-ids"))?;
    for r in &relations {
        ids.extend(r.label.into_iter().chain(r.admin_centre))?;
    }
    let sorted = use_sorted_join(path, opts.node_lookup);
    let mut join = sorted.then(|| RefSink::new(&work.path));
    let scan = reader::scan_ways(&index, &member_ways, &mut ids, join.as_mut())?;
    drop(member_ways);

    // Pass 3: coordinates, and place nodes for the locality fallback.
    let mut coords = NodeCoords::build(ids, &work.path.join("node-coords"))?;
    let mut units: Vec<AdminUnit> = Vec::new();
    let mut localities: Vec<LocalityPoint> = Vec::new();
    let mut node_units: HashMap<i64, u32> = HashMap::new();
    let (place_nodes, way_geoms) = match join {
        Some(join) => {
            let references = join.references();
            let mut joiner = join.into_joiner()?;
            let places = reader::scan_nodes_joined(&index, &mut coords, &mut joiner)?;
            let (geoms, matched) = joiner.finish(&work.path.join("way-geoms"))?;
            info!(references, matched, "way geometry joined");
            (places, Some(geoms))
        }
        None => (reader::scan_nodes(&index, &mut coords)?, None),
    };
    let place_node_info: HashMap<i64, (Layer, Option<String>)> = place_nodes
        .iter()
        .map(|n| (n.id, (n.class.layer, n.tags.name().map(str::to_string))))
        .collect();
    for n in place_nodes {
        if matches!(n.class.layer, Layer::City | Layer::District)
            && let Some(radius_m) = areas::place_radius_m(&n.class.value)
        {
            let u = unit(&mut units, &n.tags, n.class.layer);
            localities.push(LocalityPoint {
                unit: u,
                layer: n.class.layer,
                point: n.point,
                radius_m,
            });
            node_units.insert(n.id, u);
        }
    }

    // Areas: place=* ways, admin / postcode / place relations. The
    // geometry work (ring assembly, polygon indexes, interior points) runs
    // on all cores; units are then numbered sequentially, in relation
    // order, so the result does not depend on thread timing.
    let t_areas = Instant::now();
    let mut admin_areas: Vec<Area> = Vec::new();
    let mut way_units: HashMap<i64, u32> = HashMap::new();
    let way_polys: Vec<Option<(BandedPolygon, BBox)>> = scan
        .area_ways
        .par_iter()
        .map(|w| {
            let pts = coords.line(&w.refs);
            if pts.len() != w.refs.len() {
                return None;
            }
            let mp = MultiPolygon::new(vec![geo::Polygon::new(rings::to_linestring(&pts), vec![])]);
            Some((BandedPolygon::new(&mp), bbox_of(&mp)?))
        })
        .collect();
    for (w, poly) in scan.area_ways.iter().zip(way_polys) {
        if let Some((polygon, bbox)) = poly {
            let u = add_area(
                &mut units,
                &mut admin_areas,
                &w.tags,
                w.class.layer,
                11,
                polygon,
                bbox,
            );
            way_units.insert(w.id, u);
        }
    }
    let geoms: Vec<RelGeometry> = relations
        .par_iter()
        .map(|r| relation_geometry(r, &scan.member_refs, &coords))
        .collect();
    let mut postcode_names = Vec::new();
    let mut postcode_areas = Vec::new();
    let mut rel_features: Vec<RelFeature> = Vec::new();
    let mut broken = 0usize;
    // Countries with a region cut by the extract's edge: the extract holds
    // only part of them, even if their outer boundary is complete.
    let mut clipped: HashSet<String> = HashSet::new();
    for (ri, (r, g)) in relations.iter().zip(geoms).enumerate() {
        broken += g.broken as usize;
        if g.broken
            && r.is_admin_boundary()
            && let Some(cc) = r.tags.subdivision_country()
        {
            clipped.insert(cc);
        }
        let mut own = None;
        if let Some((layer, rank, polygon, bbox)) = g.admin {
            own = Some(add_area(
                &mut units,
                &mut admin_areas,
                &r.tags,
                layer,
                rank,
                polygon,
                bbox,
            ));
        }
        if let Some((code, polygon, bbox)) = g.postcode {
            // Postcode areas reuse `Area`; `unit` indexes `postcode_names`.
            postcode_names.push(code);
            postcode_areas.push(Area {
                unit: (postcode_names.len() - 1) as u32,
                layer: Layer::Locality,
                rank: 0,
                polygon,
                bbox,
                country_code: None,
            });
        }
        if let (Some(class), Some((center, extent, polygons))) = (&r.class, g.feature) {
            rel_features.push(RelFeature {
                rel: ri,
                class: class.clone(),
                center,
                extent,
                polygons,
                own,
                absorbed: false,
            });
        }
    }
    drop(scan);
    if broken > 0 {
        warn!(
            broken,
            "relations with incomplete geometry (probably clipped by the extract); they are not used for the hierarchy"
        );
    }
    // Names come from country boundaries, complete or not.
    let mut country_names: HashMap<String, String> = HashMap::new();
    for r in &relations {
        if r.is_admin_boundary()
            && r.tags.admin_level() == Some(2)
            && let (Some(cc), Some(name)) = (r.tags.country_code(), r.tags.name())
        {
            country_names.entry(cc).or_insert_with(|| name.to_string());
        }
    }
    // Countries the extract covers: a complete country boundary, or a
    // complete region of it, lying inside the extract's bounding box. A
    // neighbour's boundary can be complete in an extract that holds only a
    // strip along it (South Africa's extract has a hole for Lesotho), so a
    // country whose regions are cut counts as covered by regions only.
    let extent = reader::header_bbox(path);
    let mut covered: BTreeMap<String, sink::Coverage> = BTreeMap::new();
    for a in &admin_areas {
        if let (Some(cc), Some(e)) = (&a.country_code, &extent)
            && e.contains_bbox(&a.bbox)
        {
            let kind = if a.layer == Layer::Country && !clipped.contains(cc) {
                sink::Coverage::Boundary
            } else {
                sink::Coverage::Regions
            };
            let k = covered.entry(cc.clone()).or_insert(kind);
            *k = (*k).max(kind);
        }
    }
    info!(
        countries = %covered.iter().map(|(cc, k)| format!("{cc}({k:?})")).collect::<Vec<_>>().join(" "),
        "countries covered by the extract"
    );
    let covered: HashMap<String, sink::Coverage> = covered.into_iter().collect();
    info!(
        admin_areas = admin_areas.len(),
        postcode_areas = postcode_areas.len(),
        localities = localities.len(),
        elapsed = ?t_areas.elapsed(),
        "areas built"
    );

    // A boundary whose label (or same-named admin centre) is a matching
    // `place=*` node is the same real-world place: the node is kept and
    // gets the boundary's extent and polygon. Decided up front, from the
    // sorted place nodes, so the result does not depend on thread timing.
    let mut twins: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, f) in rel_features.iter_mut().enumerate() {
        let r = &relations[f.rel];
        let layer_ok = |id: &i64| {
            place_node_info
                .get(id)
                .is_some_and(|(l, _)| *l == f.class.layer)
        };
        let twin = r.label.filter(layer_ok).or_else(|| {
            r.admin_centre
                .filter(|id| layer_ok(id) && place_node_info[id].1.as_deref() == r.tags.name())
        });
        if let Some(node) = twin {
            twins.entry(node).or_default().push(i);
            f.absorbed = true;
        }
    }
    drop(place_node_info);

    let hierarchy = Hierarchy {
        units,
        areas: AreaIndex::new(admin_areas),
        postcodes: AreaIndex::new(postcode_areas),
        postcode_names,
        localities: LocalityIndex::new(localities),
    };
    let mut router = Router {
        sink: CountrySink::new(work.path.clone(), opts.default_country.clone()),
        streets: StreetStore::create(&work.path.join("streets.spill"))?,
        count: 0,
        started,
    };
    // Builds a place's hierarchy on a worker; returns it with its city unit.
    let finish = |cache: &mut HierCache, mut p: Place, own: Option<u32>| {
        hierarchy.assign(cache, &mut p, own);
        let city = if p.layer == Layer::Street {
            hierarchy.city_of(&p)
        } else {
            None
        };
        (p, city)
    };

    // Pass 4: way features.
    reader::stream_ways(
        &index,
        || (HierCache::default(), WayCursor::default()),
        |(cache, cursor), id, refs, t, c| {
            let pts = match &way_geoms {
                Some(geoms) => geoms.line(cursor, id),
                None => coords.line(refs),
            };
            Ok(way_place(id, refs, pts, &t, &c)
                .map(|p| finish(cache, p, way_units.get(&id).copied())))
        },
        |(p, city)| router.route(p, city),
    )?;
    drop(coords);
    drop(way_geoms);

    // Pass 5: node features.
    reader::stream_nodes(
        &index,
        HierCache::default,
        |cache, id, point, t, c| {
            let p = make_place(OsmType::Node, id, &t, &c, point);
            Ok(Some(finish(cache, p, node_units.get(&id).copied())))
        },
        |(mut place, city)| {
            for i in twins.remove(&place.osm_id).unwrap_or_default() {
                let f = &mut rel_features[i];
                place.extent = place.extent.or(f.extent);
                if place.polygons.is_empty() {
                    place.polygons = std::mem::take(&mut f.polygons);
                }
            }
            router.route(place, city)
        },
    )?;

    // Relation features that no node absorbed.
    let mut cache = HierCache::default();
    for f in rel_features.into_iter().filter(|f| !f.absorbed) {
        let Some(center) = f.center else { continue };
        let r = &relations[f.rel];
        let mut place = make_place(OsmType::Relation, r.id, &r.tags, &f.class, center);
        place.extent = f.extent;
        place.polygons = f.polygons;
        let (p, city) = finish(&mut cache, place, f.own);
        router.route(p, city)?;
    }
    // Every place has its hierarchy now: free the boundary indexes, the
    // relations and the lookup maps before street merging and writing,
    // which would otherwise run with all of that still in memory.
    // Move it into a block: moving one field out of `hierarchy` would keep
    // the others (the polygon indexes) alive until the end of the function.
    let units = {
        let h = hierarchy;
        h.units
    };
    drop(relations);
    drop(twins);
    drop(node_units);
    drop(way_units);

    let Router {
        mut sink,
        streets,
        count,
        ..
    } = router;
    let (segments, merged) = streets.finish(|p| sink.push(p))?;
    info!(
        places = count,
        street_segments = segments,
        streets = merged,
        elapsed = ?started.elapsed(),
        "features extracted"
    );
    let countries = sink.finish(opts, units, &country_names, &covered)?;
    Ok(Import {
        countries,
        _work: work,
    })
}
