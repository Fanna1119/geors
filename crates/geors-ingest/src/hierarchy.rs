//! The address hierarchy: which admin units, country and postcode a place
//! belongs to. Built during import from boundary polygons and `place=*`
//! nodes; can be kept on disk ([`Hierarchy::save`]) so incremental updates
//! assign parents to changed places exactly as the import did.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use geors_core::geom::BBox;
use geors_core::{AdminUnit, Layer, OsmType, Place};
use serde::{Deserialize, Serialize};

use crate::areas::{Area, AreaIndex, CellCache, LocalityIndex, LocalityPoint};
use crate::pip::PolygonStore;

/// Files of a kept hierarchy (in its state directory).
pub const ADMIN_POLYGONS: &str = "admin-polygons";
pub const POSTCODE_POLYGONS: &str = "postcode-polygons";
const META: &str = "hierarchy.bin";
/// Version of the kept hierarchy files; older ones need a full import.
const VERSION: u32 = 1;
/// Admin units, as JSON like a partition's `admin.json` (`AdminUnit` skips
/// empty fields, which a non-self-describing format cannot decode).
const UNITS: &str = "units.json";

/// Everything but the units and the polygon arrays (those are in the
/// polygon stores).
#[derive(Serialize, Deserialize)]
struct Meta {
    version: u32,
    areas: Vec<Area>,
    postcodes: Vec<Area>,
    postcode_names: Vec<String>,
    localities: Vec<LocalityPoint>,
    owners: Vec<(OsmType, i64, u32)>,
    postcode_owners: Vec<(OsmType, i64, u32)>,
}

/// Assigns parents (admin units), country and postcode to places.
pub struct Hierarchy {
    pub units: Vec<AdminUnit>,
    pub areas: AreaIndex,
    pub postcodes: AreaIndex,
    pub postcode_names: Vec<String>,
    pub localities: LocalityIndex,
    /// The unit each area element defines (a boundary relation, a `place=*`
    /// way or node): a place is not its own parent.
    pub owners: HashMap<(OsmType, i64), u32>,
    /// The postcode area (index in `postcodes`) each element defines.
    pub postcode_owners: HashMap<(OsmType, i64), u32>,
    /// Changed areas laid over this hierarchy (see [`Hierarchy::with_changes`]).
    pub(crate) overlay: Option<Box<Overlay>>,
}

/// Areas of changed elements, from the small import of an incremental
/// update, replacing those of the same elements in the base hierarchy.
pub(crate) struct Overlay {
    local: Hierarchy,
    /// Local unit `u` is unit `u + unit_offset` of the combined hierarchy.
    unit_offset: u32,
    /// Local units and postcode areas that belong to changed elements (the
    /// rest of the small import's areas are unchanged dependencies).
    units: HashSet<u32>,
    postcodes: HashSet<u32>,
    /// Base units and postcode areas of changed elements.
    removed_units: HashSet<u32>,
    removed_postcodes: HashSet<u32>,
    /// Old and new polygons per changed element.
    changes: Vec<AreaChange>,
}

/// Which polygon set an area index refers to.
#[derive(Clone, Copy)]
enum Set {
    BaseAreas,
    BasePostcodes,
    LocalAreas,
    LocalPostcodes,
}

/// A changed area: the polygons of one element before and after.
pub struct AreaChange {
    /// Extent of the old and new polygons together.
    pub bbox: BBox,
    /// Where membership can differ: boxes around the edges that changed
    /// (a new or deleted area: its whole extent).
    pub regions: Vec<BBox>,
    old: Vec<(Set, usize)>,
    new: Vec<(Set, usize)>,
}

/// Most changed-edge boxes merged pairwise; beyond, one box is used.
const MAX_EDGE_BOXES: usize = 2000;

/// Boxes around the edges that are in only one of `old` and `new`, merged
/// where they touch. A region whose membership changed is enclosed by a
/// loop of changed edges, which share endpoints and so end up in one box.
fn changed_edge_regions(
    old: Vec<([i32; 2], [i32; 2])>,
    new: Vec<([i32; 2], [i32; 2])>,
) -> Vec<BBox> {
    let norm = |(a, b): ([i32; 2], [i32; 2])| if a <= b { (a, b) } else { (b, a) };
    let old: HashSet<_> = old.into_iter().map(norm).collect();
    let new: HashSet<_> = new.into_iter().map(norm).collect();
    let e7 = |v: i32| v as f64 * 1e-7;
    let mut boxes: Vec<BBox> = old
        .symmetric_difference(&new)
        .map(|(a, b)| {
            // A margin of ~1 m, so touching edges overlap.
            BBox::new(
                e7(a[0].min(b[0])) - 1e-5,
                e7(a[1].min(b[1])) - 1e-5,
                e7(a[0].max(b[0])) + 1e-5,
                e7(a[1].max(b[1])) + 1e-5,
            )
        })
        .collect();
    if boxes.len() > MAX_EDGE_BOXES {
        let mut all = BBox::empty();
        for b in &boxes {
            all.union(b);
        }
        return vec![all];
    }
    // Merge until no two boxes intersect.
    let mut merged = true;
    while merged {
        merged = false;
        let mut out: Vec<BBox> = Vec::with_capacity(boxes.len());
        for b in boxes {
            match out.iter_mut().find(|o| o.intersects(&b)) {
                Some(o) => {
                    o.union(&b);
                    merged = true;
                }
                None => out.push(b),
            }
        }
        boxes = out;
    }
    boxes
}

/// Membership of a grid cell in a changed area, before and after.
#[derive(Clone, Copy, PartialEq)]
pub enum CellChange {
    /// No polygon edge crosses the cell, and membership is the same.
    Unchanged,
    /// No edge crosses the cell, and membership differs: every point in it
    /// changed.
    Changed,
    /// Some edge crosses the cell: test points one by one.
    Mixed,
}

/// Extent of an area, for preferring the more specific of overlapping
/// areas.
fn area_size(b: &BBox) -> f64 {
    (b.max_lon - b.min_lon) * (b.max_lat - b.min_lat)
}

/// Per-worker point-in-polygon caches.
#[derive(Default)]
pub struct HierCache {
    areas: CellCache,
    postcodes: CellCache,
    overlay_areas: CellCache,
    overlay_postcodes: CellCache,
}

impl Hierarchy {
    /// Read-only apart from the caller's cache, so workers run it in parallel.
    pub fn assign(&self, cache: &mut HierCache, place: &mut Place, own: Option<u32>) {
        let o = self.overlay.as_deref();
        let removed = |u: u32| o.is_some_and(|o| o.removed_units.contains(&u));
        // Per layer: (rank, size, unit). Among areas of equal rank the
        // smaller one wins, then the name: the result does not depend on the
        // order areas are found in (which differs between a full import and
        // an incremental update).
        let mut best: BTreeMap<Layer, (u8, f64, u32)> = BTreeMap::new();
        let mut country = None;
        let mut region_country = None;
        let units = &self.units;
        let mut consider = |a: &Area, unit: u32| {
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
            if a.layer <= place.layer || Some(unit) == own {
                return;
            }
            let size = area_size(&a.bbox);
            let cand = (a.rank, size, unit);
            let e = best.entry(a.layer).or_insert(cand);
            let name = |u: u32| units.get(u as usize).map(|x| x.name.as_str());
            let better = a.rank > e.0
                || (a.rank == e.0 && (size < e.1 || (size == e.1 && name(unit) < name(e.2))));
            if better {
                *e = cand;
            }
        };
        for ai in self.areas.containing(&mut cache.areas, place.center) {
            let a = &self.areas.areas[ai];
            if !removed(a.unit) {
                consider(a, a.unit);
            }
        }
        if let Some(o) = o {
            for ai in o
                .local
                .areas
                .containing(&mut cache.overlay_areas, place.center)
            {
                let a = &o.local.areas.areas[ai];
                if o.units.contains(&a.unit) {
                    consider(a, a.unit + o.unit_offset);
                }
            }
        }
        for layer in [Layer::City, Layer::District] {
            if place.layer < layer && !best.contains_key(&layer) {
                let base = self
                    .localities
                    .best_where(place.center, layer, |u| !removed(u) && Some(u) != own);
                let changed = o.and_then(|o| {
                    o.local
                        .localities
                        .best_where(place.center, layer, |u| {
                            o.units.contains(&u) && Some(u + o.unit_offset) != own
                        })
                        .map(|(r, u)| (r, u + o.unit_offset))
                });
                let pick = match (base, changed) {
                    (Some(b), Some(c)) => Some(if c.0 < b.0 { c } else { b }),
                    (b, c) => b.or(c),
                };
                if let Some((_, u)) = pick {
                    best.insert(layer, (0, 0.0, u));
                }
            }
        }
        place.parents = best.values().map(|&(_, _, u)| u).collect();
        // A country boundary cut by the extract's edge is unusable, but the
        // regions inside it (states, provinces) usually are complete and
        // name their country in `ISO3166-2`.
        place.country_code = country.or(region_country).or(place.country_code.take());
        if place.postcode.is_none() && place.layer <= Layer::Street {
            // The smallest containing postcode area, then by code (overlaps
            // along borders; independent of the order areas are found in).
            let mut found: Option<(f64, String)> = None;
            let mut offer = |size: f64, code: &str| {
                if found
                    .as_ref()
                    .is_none_or(|(s, c)| size < *s || (size == *s && code < c.as_str()))
                {
                    found = Some((size, code.to_string()));
                }
            };
            for i in self
                .postcodes
                .containing(&mut cache.postcodes, place.center)
            {
                if !o.is_some_and(|o| o.removed_postcodes.contains(&(i as u32))) {
                    let a = &self.postcodes.areas[i];
                    offer(area_size(&a.bbox), &self.postcode_names[a.unit as usize]);
                }
            }
            if let Some(o) = o {
                for i in o
                    .local
                    .postcodes
                    .containing(&mut cache.overlay_postcodes, place.center)
                {
                    if o.postcodes.contains(&(i as u32)) {
                        let a = &o.local.postcodes.areas[i];
                        offer(area_size(&a.bbox), &o.local.postcode_names[a.unit as usize]);
                    }
                }
            }
            place.postcode = found.map(|(_, code)| code);
        }
    }

    /// This hierarchy with the areas of `changed` elements replaced by those
    /// in `local` (built by the small import of an incremental update):
    /// changed or deleted boundaries, postcode areas and `place=*` areas
    /// and nodes. Units of new areas are appended to `units`.
    pub fn with_changes(mut self, local: Hierarchy, changed: &HashSet<(OsmType, i64)>) -> Self {
        // A place node whose locality is the same before and after (same
        // position, reach, layer and names) changes nothing: leave it out.
        fn point_of(h: &Hierarchy) -> HashMap<u32, &LocalityPoint> {
            h.localities.points.iter().map(|l| (l.unit, l)).collect()
        }
        let (base_points, local_points) = (point_of(&self), point_of(&local));
        let same_locality = |k: &(OsmType, i64)| {
            let (Some(&a), Some(&b)) = (self.owners.get(k), local.owners.get(k)) else {
                return false;
            };
            let (Some(pa), Some(pb)) = (base_points.get(&a), local_points.get(&b)) else {
                return false;
            };
            pa.point == pb.point
                && pa.radius_m == pb.radius_m
                && pa.layer == pb.layer
                && self.units.get(a as usize) == local.units.get(b as usize)
        };
        let changed: HashSet<(OsmType, i64)> = changed
            .iter()
            .filter(|k| !same_locality(k))
            .copied()
            .collect();
        let changed = &changed;
        let pick = |m: &HashMap<(OsmType, i64), u32>| -> HashSet<u32> {
            changed.iter().filter_map(|k| m.get(k)).copied().collect()
        };
        let removed_units = pick(&self.owners);
        let removed_postcodes = pick(&self.postcode_owners);
        let units = pick(&local.owners);
        let postcodes = pick(&local.postcode_owners);
        let mut area_changes = Vec::new();
        for k in changed {
            let mut c = AreaChange {
                bbox: BBox::empty(),
                regions: Vec::new(),
                old: Vec::new(),
                new: Vec::new(),
            };
            let mut add = |list: &mut Vec<(Set, usize)>, set: Set, index: &AreaIndex, i: usize| {
                c.bbox.union(&index.areas[i].bbox);
                list.push((set, i));
            };
            if let Some(&u) = self.owners.get(k) {
                for (i, a) in self.areas.areas.iter().enumerate() {
                    if a.unit == u {
                        add(&mut c.old, Set::BaseAreas, &self.areas, i);
                    }
                }
            }
            if let Some(&i) = self.postcode_owners.get(k) {
                add(&mut c.old, Set::BasePostcodes, &self.postcodes, i as usize);
            }
            if let Some(&u) = local.owners.get(k) {
                for (i, a) in local.areas.areas.iter().enumerate() {
                    if a.unit == u {
                        add(&mut c.new, Set::LocalAreas, &local.areas, i);
                    }
                }
            }
            if let Some(&i) = local.postcode_owners.get(k) {
                add(
                    &mut c.new,
                    Set::LocalPostcodes,
                    &local.postcodes,
                    i as usize,
                );
            }
            if c.old.is_empty() || c.new.is_empty() {
                // New or deleted: the whole area.
                c.regions = vec![c.bbox];
            } else {
                let edges = |list: &[(Set, usize)], base: &Hierarchy, local: &Hierarchy| {
                    list.iter()
                        .flat_map(|&(set, i)| {
                            let ix = match set {
                                Set::BaseAreas => &base.areas,
                                Set::BasePostcodes => &base.postcodes,
                                Set::LocalAreas => &local.areas,
                                Set::LocalPostcodes => &local.postcodes,
                            };
                            ix.store().edges(&ix.areas[i].polygon)
                        })
                        .collect::<Vec<_>>()
                };
                c.regions = changed_edge_regions(
                    edges(&c.old, &self, &local),
                    edges(&c.new, &self, &local),
                );
            }
            if !c.old.is_empty() || !c.new.is_empty() {
                area_changes.push(c);
            }
        }
        let unit_offset = self.units.len() as u32;
        self.units.extend(local.units.iter().cloned());
        for k in changed {
            self.owners.remove(k);
            self.postcode_owners.remove(k);
            if let Some(&u) = local.owners.get(k) {
                self.owners.insert(*k, u + unit_offset);
            }
        }
        self.overlay = Some(Box::new(Overlay {
            local,
            unit_offset,
            units,
            postcodes,
            removed_units,
            removed_postcodes,
            changes: area_changes,
        }));
        self
    }

    /// The changed areas (polygons per changed element, before and after).
    pub fn area_changes(&self) -> &[AreaChange] {
        self.overlay.as_deref().map_or(&[], |o| &o.changes)
    }

    fn index(&self, set: Set) -> Option<&AreaIndex> {
        let o = self.overlay.as_deref();
        Some(match set {
            Set::BaseAreas => &self.areas,
            Set::BasePostcodes => &self.postcodes,
            Set::LocalAreas => &o?.local.areas,
            Set::LocalPostcodes => &o?.local.postcodes,
        })
    }

    fn any_contains(&self, list: &[(Set, usize)], p: geors_core::LonLat) -> bool {
        list.iter().any(|&(set, i)| {
            self.index(set)
                .is_some_and(|ix| ix.store().contains(&ix.areas[i].polygon, p.lon, p.lat))
        })
    }

    /// Whether `p` is in the changed area before and after.
    pub fn membership(&self, c: &AreaChange, p: geors_core::LonLat) -> (bool, bool) {
        (self.any_contains(&c.old, p), self.any_contains(&c.new, p))
    }

    /// Whether membership can differ inside `cell` (see [`CellChange`]).
    pub fn cell_change(&self, c: &AreaChange, cell: &BBox) -> CellChange {
        let crosses = c.old.iter().chain(&c.new).any(|&(set, i)| {
            self.index(set)
                .is_some_and(|ix| ix.store().crosses(&ix.areas[i].polygon, cell))
        });
        if crosses {
            return CellChange::Mixed;
        }
        let (old, new) = self.membership(c, cell.center());
        if old == new {
            CellChange::Unchanged
        } else {
            CellChange::Changed
        }
    }

    /// The city and district a point would get from the `place=*` node
    /// fallback (ignoring boundaries), as unit names: for finding where
    /// changed place nodes change the result.
    pub fn fallback_at(&self, p: geors_core::LonLat) -> [Option<String>; 2] {
        let o = self.overlay.as_deref();
        [Layer::City, Layer::District].map(|layer| {
            let base = self.localities.best_where(p, layer, |u| {
                !o.is_some_and(|o| o.removed_units.contains(&u))
            });
            let changed = o.and_then(|o| {
                o.local
                    .localities
                    .best_where(p, layer, |u| o.units.contains(&u))
                    .map(|(r, u)| (r, u + o.unit_offset))
            });
            let pick = match (base, changed) {
                (Some(b), Some(c)) => Some(if c.0 < b.0 { c } else { b }),
                (b, c) => b.or(c),
            };
            pick.and_then(|(_, u)| self.units.get(u as usize))
                .map(|u| u.name.clone())
        })
    }

    /// Reach of changed `place=*` nodes (the city / district fallback),
    /// before and after: centre and radius in metres.
    pub fn locality_changes(&self) -> Vec<(geors_core::LonLat, f64)> {
        let Some(o) = self.overlay.as_deref() else {
            return Vec::new();
        };
        let reach = |l: &LocalityPoint| (l.point, l.radius_m);
        self.localities
            .points
            .iter()
            .filter(|l| o.removed_units.contains(&l.unit))
            .map(reach)
            .chain(
                o.local
                    .localities
                    .points
                    .iter()
                    .filter(|l| o.units.contains(&l.unit))
                    .map(reach),
            )
            .collect()
    }

    pub fn city_of(&self, place: &Place) -> Option<u32> {
        place
            .parents
            .iter()
            .copied()
            .find(|&u| self.units[u as usize].layer == Layer::City)
    }
}

impl std::fmt::Debug for Hierarchy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hierarchy")
            .field("units", &self.units.len())
            .field("areas", &self.areas.areas.len())
            .field("postcodes", &self.postcodes.areas.len())
            .field("localities", &self.localities.points.len())
            .finish()
    }
}

impl Hierarchy {
    /// Save the metadata to `dir`. The polygon stores must have been created
    /// there with [`PolygonStore::create_kept`] ([`ADMIN_POLYGONS`],
    /// [`POSTCODE_POLYGONS`]).
    pub fn save(&self, dir: &Path) -> Result<()> {
        let units_tmp = dir.join(format!("{UNITS}.tmp"));
        std::fs::write(&units_tmp, serde_json::to_vec(&self.units)?)?;
        std::fs::rename(&units_tmp, dir.join(UNITS))?;
        let sorted = |m: &HashMap<(OsmType, i64), u32>| {
            let mut v: Vec<(OsmType, i64, u32)> =
                m.iter().map(|(&(t, id), &u)| (t, id, u)).collect();
            v.sort_unstable_by_key(|&(t, id, _)| (t as u8, id));
            v
        };
        let meta = Meta {
            version: VERSION,
            areas: self.areas.areas.clone(),
            postcodes: self.postcodes.areas.clone(),
            postcode_names: self.postcode_names.clone(),
            localities: self.localities.points.clone(),
            owners: sorted(&self.owners),
            postcode_owners: sorted(&self.postcode_owners),
        };
        let tmp = dir.join(format!("{META}.tmp"));
        std::fs::write(&tmp, postcard::to_allocvec(&meta)?)?;
        std::fs::rename(&tmp, dir.join(META))?;
        Ok(())
    }

    /// Whether `dir` holds a hierarchy saved with [`Hierarchy::save`].
    pub fn is_kept(dir: &Path) -> bool {
        dir.join(META).is_file() && dir.join(UNITS).is_file()
    }

    /// Load a hierarchy saved with [`Hierarchy::save`].
    pub fn load(dir: &Path) -> Result<Self> {
        let bytes = std::fs::read(dir.join(META))
            .with_context(|| format!("no kept hierarchy in '{}'", dir.display()))?;
        let meta: Meta = postcard::from_bytes(&bytes)
            .ok()
            .filter(|m: &Meta| m.version == VERSION)
            .context("kept hierarchy is from another version; run a full import")?;
        let units: Vec<AdminUnit> = serde_json::from_slice(&std::fs::read(dir.join(UNITS))?)
            .context("corrupt kept admin units")?;
        Ok(Self {
            units,
            areas: AreaIndex::new(meta.areas, PolygonStore::open(&dir.join(ADMIN_POLYGONS))?)?,
            postcodes: AreaIndex::new(
                meta.postcodes,
                PolygonStore::open(&dir.join(POSTCODE_POLYGONS))?,
            )?,
            postcode_names: meta.postcode_names,
            localities: LocalityIndex::new(meta.localities),
            owners: meta
                .owners
                .into_iter()
                .map(|(t, id, u)| ((t, id), u))
                .collect(),
            postcode_owners: meta
                .postcode_owners
                .into_iter()
                .map(|(t, id, u)| ((t, id), u))
                .collect(),
            overlay: None,
        })
    }

    /// The unit `element` defines, if it is an area of this hierarchy.
    pub fn owner(&self, kind: OsmType, id: i64) -> Option<u32> {
        self.owners.get(&(kind, id)).copied()
    }
}
