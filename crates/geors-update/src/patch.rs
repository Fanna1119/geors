//! Incremental updates: turn the changes since a base PBF into patch
//! partitions, without re-importing the extract.
//!
//! The affected places are rebuilt by the normal importer: their elements,
//! and everything those reference, are written to a small PBF that is
//! imported with the kept address hierarchy (see `docs/incremental-updates.md`).
//! The patch is always built from all changes since the base, so it never
//! depends on an earlier patch.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use geors_core::geom::{BBox, LonLat};
use geors_core::spill::{SpillReader, SpillWriter};
use geors_core::{Layer, OsmType};
use geors_index::Partition;
use geors_index::text::osm_key;
use geors_ingest::hierarchy::Hierarchy;
use geors_ingest::{ImportOptions, NodeLookup};
use rayon::prelude::*;
use tracing::info;

use crate::elements::{BlockIndex, ElementSource};
use crate::osm::{ChangeSet, Elem, Kind};
use crate::pbf_write::{blob, header_block, primitive_block};

/// What a patch run did.
#[derive(Debug, Default)]
pub struct PatchStats {
    /// Elements whose places are rebuilt (changed, reshaped, completed).
    pub roots: usize,
    /// Elements in the small extract (roots and what they reference).
    pub elements: usize,
    /// Places in the patch partitions.
    pub places: u64,
    /// Main-partition places deleted or replaced.
    pub tombstones: usize,
    /// Countries patched.
    pub countries: Vec<String>,
}

/// Roots: the elements whose places are rebuilt.
#[derive(Default)]
struct Roots {
    nodes: BTreeSet<i64>,
    ways: BTreeSet<i64>,
    relations: BTreeSet<i64>,
}

impl Roots {
    fn contains(&self, t: OsmType, id: i64) -> bool {
        match t {
            OsmType::Node => self.nodes.contains(&id),
            OsmType::Way => self.ways.contains(&id),
            OsmType::Relation => self.relations.contains(&id),
        }
    }

    fn insert(&mut self, t: OsmType, id: i64) {
        match t {
            OsmType::Node => self.nodes.insert(id),
            OsmType::Way => self.ways.insert(id),
            OsmType::Relation => self.relations.insert(id),
        };
    }

    fn iter(&self) -> impl Iterator<Item = (OsmType, i64)> + '_ {
        let nodes = self.nodes.iter().map(|&id| (OsmType::Node, id));
        let ways = self.ways.iter().map(|&id| (OsmType::Way, id));
        let relations = self.relations.iter().map(|&id| (OsmType::Relation, id));
        nodes.chain(ways).chain(relations)
    }

    fn len(&self) -> usize {
        self.nodes.len() + self.ways.len() + self.relations.len()
    }

    fn keys(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.len());
        for (t, ids) in [
            (OsmType::Node, &self.nodes),
            (OsmType::Way, &self.ways),
            (OsmType::Relation, &self.relations),
        ] {
            out.extend(ids.iter().map(|&id| osm_key(t, id)));
        }
        out
    }
}

fn tag<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Named highways and waterways: merged across segments by the importer.
fn street_like(e: &Elem) -> bool {
    matches!(e, Elem::Way { tags, .. }
        if tag(tags, "name").is_some() && (tag(tags, "highway").is_some() || tag(tags, "waterway").is_some()))
}

/// Ids of a place: its own and the segments merged into it.
fn place_ids(p: &geors_core::Place) -> impl Iterator<Item = (OsmType, i64)> + '_ {
    std::iter::once((p.osm_type, p.osm_id)).chain(p.merged_ids.iter().map(|&id| (p.osm_type, id)))
}

/// Build patch partitions in `data_dir` for all `changes` since `base`,
/// with the address hierarchy kept in `state_dir` by the base's import.
pub fn build_patch(
    base: &Path,
    changes: &ChangeSet,
    state_dir: &Path,
    data_dir: &Path,
    work: &Path,
) -> Result<PatchStats> {
    let t = Instant::now();
    std::fs::create_dir_all(work)?;
    let index = BlockIndex::load_or_build(base)?;
    let source = ElementSource::new(base, &index, changes);
    let parts: Vec<Partition> = geors_index::engine::list_partitions(data_dir)?
        .into_iter()
        .map(|cc| Partition::open(&data_dir.join(cc)).map_err(anyhow::Error::from))
        .collect::<Result<_>>()?;
    let mut stats = PatchStats::default();

    // Roots: changed elements, ways using a changed node, relations using a
    // root way or changed node.
    let mut roots = Roots::default();
    roots.nodes.extend(changes.nodes.keys());
    roots.ways.extend(changes.ways.keys());
    roots.relations.extend(changes.relations.keys());
    let changed_nodes: HashSet<i64> = changes.nodes.keys().copied().collect();
    roots.ways.extend(source.scan(
        Kind::Way,
        |e| matches!(e, Elem::Way { refs, .. } if refs.iter().any(|r| changed_nodes.contains(r))),
    )?);
    let root_ways: HashSet<i64> = roots.ways.iter().copied().collect();
    roots.relations.extend(source.scan(Kind::Relation, |e| {
        matches!(e, Elem::Relation { members, .. } if members.iter().any(|m| match m.kind {
            Kind::Way => root_ways.contains(&m.id),
            Kind::Node => changed_nodes.contains(&m.id),
            Kind::Relation => false,
        }))
    })?);
    // Relations naming a label / admin_centre node: a rebuilt place node
    // takes over its boundary's polygon and extent, so that relation goes
    // into the extract with it.
    let labelled = source.scan(Kind::Relation, |e| {
        matches!(e, Elem::Relation { members, .. }
            if members.iter().any(|m| m.kind == Kind::Node && matches!(m.role.as_str(), "label" | "admin_centre")))
    })?;
    let labelled: Vec<i64> = labelled.into_iter().collect();
    let mut twins: HashMap<i64, Vec<i64>> = HashMap::new();
    for r in source.get(Kind::Relation, &labelled)?.values() {
        if let Elem::Relation { id, members, .. } = r {
            for m in members {
                if m.kind == Kind::Node && matches!(m.role.as_str(), "label" | "admin_centre") {
                    twins.entry(m.id).or_default().push(*id);
                }
            }
        }
    }
    info!(ways = roots.ways.len(), relations = roots.relations.len(), elapsed = ?t.elapsed(), "patch: roots found");

    // Phase A: import the roots with the kept hierarchy.
    let kept = Arc::new(Hierarchy::load(state_dir)?);
    let (mut import, mut elements) =
        import_roots(&source, &parts, &mut roots, &twins, kept.clone(), work, "a")?;

    // Changed areas (boundaries, postcode areas, place=* areas and nodes):
    // a hierarchy with them replaced, and the main places whose parents,
    // country or postcode differ under it become roots too (phase B).
    let local = import.local_hierarchy.take();
    let changed: HashSet<(OsmType, i64)> = roots
        .iter()
        .filter(|k| {
            let defines =
                |h: &Hierarchy| h.owners.contains_key(k) || h.postcode_owners.contains_key(k);
            defines(&kept) || local.as_ref().is_some_and(defines)
        })
        .collect();
    if let (false, Some(local)) = (changed.is_empty(), local) {
        let patched = Arc::new(Hierarchy::load(state_dir)?.with_changes(local, &changed));
        let affected = affected_places(&parts, &kept, &patched)?;
        info!(areas = changed.len(), places = affected.len(), elapsed = ?t.elapsed(), "patch: places under changed areas");
        for (t, id) in affected {
            roots.insert(t, id);
        }
        // `patched` refers to phase A's scratch files: import again first.
        let (b, n) = import_roots(&source, &parts, &mut roots, &twins, patched, work, "b")?;
        import = b;
        elements = n;
    }
    stats.roots = roots.len();
    stats.elements = elements;

    // Per country: the rebuilt places of roots, and tombstones for the old
    // places of roots and of everything rebuilt.
    let mut patched: BTreeMap<String, (PathBuf, u64)> = BTreeMap::new();
    let mut rebuilt_keys: BTreeSet<String> = BTreeSet::new();
    for c in &import.countries {
        let reader = SpillReader::open(&c.places)?;
        let path = work.join(format!("patch-{}.spill", c.country_code));
        let mut out = SpillWriter::create(&path)?;
        let mut result = Ok(());
        reader.for_each_entry(|e| {
            if result.is_err() {
                return;
            }
            result = (|| -> Result<()> {
                let p = reader.get(e.offset)?;
                if place_ids(&p).any(|(t, id)| roots.contains(t, id)) {
                    rebuilt_keys.extend(place_ids(&p).map(|(t, id)| osm_key(t, id)));
                    out.push(&p)?;
                }
                Ok(())
            })();
        })?;
        result?;
        let spill = out.finish()?;
        patched.insert(c.country_code.clone(), (spill.path, spill.count));
    }
    let admins = import
        .countries
        .first()
        .map(|c| c.admins.clone())
        .unwrap_or_default();
    let mut keys = roots.keys();
    keys.extend(rebuilt_keys);
    keys.sort_unstable();
    keys.dedup();
    for part in &parts {
        let cc = part.code().to_string();
        let tombstones = part.text.lookup(&keys)?;
        let (path, count) = patched.remove(&cc).unwrap_or_else(|| {
            let path = work.join(format!("patch-{cc}-empty.spill"));
            (path, 0)
        });
        if count == 0 && tombstones.is_empty() {
            continue;
        }
        if count == 0 {
            SpillWriter::create(&path)?.finish()?;
        }
        stats.places += count;
        stats.tombstones += tombstones.len();
        stats.countries.push(cc.clone());
        geors_index::write_patch(
            data_dir,
            geors_index::PartitionInput {
                country_code: cc.clone(),
                country_name: part.meta.country_name.clone(),
                source: part.meta.source.clone(),
                places: geors_core::spill::Spill { path, count },
                admins: admins.clone(),
            },
            &tombstones,
        )
        .with_context(|| format!("cannot write the patch of '{cc}'"))?;
    }
    for (cc, (_, count)) in patched {
        // Places in a country without a partition (not imported before).
        info!(country = %cc, places = count, "patch: no partition for this country; skipped");
    }
    geors_core::storage::bump_generation(data_dir)?;
    info!(
        roots = stats.roots,
        elements = stats.elements,
        places = stats.places,
        tombstones = stats.tombstones,
        elapsed = ?t.elapsed(),
        "patch written"
    );
    Ok(stats)
}

/// Complete the roots (label nodes of root relations, all segments of
/// affected streets), write them and everything they reference to a small
/// PBF, and import it with `hierarchy`. Returns the import and the number
/// of elements in the extract.
fn import_roots(
    source: &ElementSource,
    parts: &[Partition],
    roots: &mut Roots,
    twins: &HashMap<i64, Vec<i64>>,
    hierarchy: Arc<Hierarchy>,
    work: &Path,
    phase: &str,
) -> Result<(geors_ingest::Import, usize)> {
    // Relations whose label / admin_centre is a root node, and the label
    // nodes of root relations: a boundary and its label node make one
    // place, so both are rebuilt together.
    let twin_rels: Vec<i64> = roots
        .nodes
        .iter()
        .filter_map(|n| twins.get(n))
        .flatten()
        .copied()
        .collect();
    roots.relations.extend(twin_rels);
    let rel_ids: Vec<i64> = roots.relations.iter().copied().collect();
    let relations = source.get(Kind::Relation, &rel_ids)?;
    for r in relations.values() {
        if let Elem::Relation { members, .. } = r {
            for m in members {
                if m.kind == Kind::Node && matches!(m.role.as_str(), "label" | "admin_centre") {
                    roots.nodes.insert(m.id);
                }
            }
        }
    }

    // Street completion: a changed segment is merged with the other
    // segments of its street, so all of them are rebuilt. Old merged places
    // that contain a root way, and same-named merged places close to a
    // changed street segment.
    let way_ids: Vec<i64> = roots.ways.iter().copied().collect();
    let mut ways = source.get(Kind::Way, &way_ids)?;
    let street_nodes: Vec<i64> = ways
        .values()
        .filter(|w| street_like(w))
        .flat_map(|w| match w {
            Elem::Way { refs, .. } => refs.clone(),
            _ => Vec::new(),
        })
        .collect();
    let street_coords = source.get(Kind::Node, &street_nodes)?;
    let mut extra: BTreeSet<i64> = BTreeSet::new();
    let root_keys: Vec<String> = roots
        .ways
        .iter()
        .map(|&id| osm_key(OsmType::Way, id))
        .collect();
    for part in parts {
        let mut add = |p: &geors_core::Place| {
            if p.layer == Layer::Street || p.osm_key == "waterway" {
                extra.extend(place_ids(p).map(|(_, id)| id));
            }
        };
        for id in part.text.lookup(&root_keys)? {
            if let Some(rec) = part.record(id) {
                add(&part.doc(&rec, false)?);
            }
        }
        for w in ways.values().filter(|w| street_like(w)) {
            let Elem::Way { refs, tags, .. } = w else {
                continue;
            };
            let name = tag(tags, "name").unwrap_or_default().to_lowercase();
            let mut b = BBox::empty();
            for r in refs {
                if let Some(Elem::Node { lon, lat, .. }) = street_coords.get(r) {
                    b.extend(LonLat::new(*lon as f64 * 1e-7, *lat as f64 * 1e-7));
                }
            }
            if b.is_empty() {
                continue;
            }
            let near = BBox::new(
                b.min_lon - 0.002,
                b.min_lat - 0.002,
                b.max_lon + 0.002,
                b.max_lat + 0.002,
            );
            for id in part.search_bbox(&near)? {
                let Some(rec) = part.record(id) else { continue };
                if rec.layer != Layer::Street && rec.layer != Layer::Poi {
                    continue;
                }
                let p = part.doc(&rec, false)?;
                if p.name.as_deref().map(str::to_lowercase).as_deref() == Some(name.as_str()) {
                    add(&p);
                }
            }
        }
    }
    let extra: Vec<i64> = extra
        .into_iter()
        .filter(|id| !roots.ways.contains(id))
        .collect();
    ways.extend(source.get(Kind::Way, &extra)?);
    roots.ways.extend(extra);

    // Everything the roots reference: member ways of relations, and the
    // nodes of all ways.
    let member_ways: Vec<i64> = relations
        .values()
        .flat_map(|r| match r {
            Elem::Relation { members, .. } => members
                .iter()
                .filter(|m| m.kind == Kind::Way)
                .map(|m| m.id)
                .collect(),
            _ => Vec::new(),
        })
        .filter(|id| !ways.contains_key(id))
        .collect();
    ways.extend(source.get(Kind::Way, &member_ways)?);
    let mut node_ids: Vec<i64> = roots.nodes.iter().copied().collect();
    for w in ways.values() {
        if let Elem::Way { refs, .. } = w {
            node_ids.extend(refs);
        }
    }
    for r in relations.values() {
        if let Elem::Relation { members, .. } = r {
            node_ids.extend(
                members
                    .iter()
                    .filter(|m| m.kind == Kind::Node)
                    .map(|m| m.id),
            );
        }
    }
    let nodes = source.get(Kind::Node, &node_ids)?;
    let elements = nodes.len() + ways.len() + relations.len();

    let extract = work.join(format!("patch-extract-{phase}.osm.pbf"));
    write_extract(&extract, &nodes, &ways, &relations)?;
    drop((nodes, ways, relations));
    let opts = ImportOptions {
        all_countries: true,
        work_dir: Some(work.to_path_buf()),
        node_lookup: NodeLookup::Memory,
        hierarchy: Some(hierarchy),
        ..Default::default()
    };
    Ok((geors_ingest::import_pbf(&extract, &opts)?, elements))
}

/// Main places whose parents, country or postcode differ between `kept`
/// and `patched` (which has the changed areas replaced).
fn affected_places(
    parts: &[Partition],
    kept: &Hierarchy,
    patched: &Hierarchy,
) -> Result<HashSet<(OsmType, i64)>> {
    use geors_ingest::hierarchy::{CellChange, HierCache};
    const CELL: f64 = 0.02;
    let t = Instant::now();
    let region_count: usize = patched.area_changes().iter().map(|c| c.regions.len()).sum();
    let mut candidates: Vec<(usize, u32)> = Vec::new();
    // Lines (merged streets, rivers) with a vertex whose membership changed:
    // the import assigns their parents per segment, so they are rebuilt
    // without comparing.
    let mut lines: Vec<(usize, u32)> = Vec::new();
    for (pi, part) in parts.iter().enumerate() {
        let mut ids: BTreeSet<u32> = BTreeSet::new();
        let mut line_ids: BTreeSet<u32> = BTreeSet::new();
        // Places whose membership in a changed area differs, decided per
        // grid cell where no edge crosses it.
        for change in patched.area_changes() {
            let mut cells: HashMap<(i32, i32), CellChange> = HashMap::new();
            let mut differs = |c: LonLat| {
                let key = ((c.lon / CELL).floor() as i32, (c.lat / CELL).floor() as i32);
                let state = *cells.entry(key).or_insert_with(|| {
                    let cell = BBox::new(
                        key.0 as f64 * CELL,
                        key.1 as f64 * CELL,
                        (key.0 + 1) as f64 * CELL,
                        (key.1 + 1) as f64 * CELL,
                    );
                    patched.cell_change(change, &cell)
                });
                match state {
                    CellChange::Unchanged => false,
                    CellChange::Changed => true,
                    CellChange::Mixed => {
                        let (old, new) = patched.membership(change, c);
                        old != new
                    }
                }
            };
            let mut seen: HashSet<u32> = HashSet::new();
            let ids_in = change
                .regions
                .iter()
                .map(|r| part.search_bbox(r))
                .collect::<Result<Vec<_>, _>>()?;
            for id in ids_in.into_iter().flatten().filter(|id| seen.insert(*id)) {
                let Some(rec) = part.record(id) else { continue };
                if differs(rec.center()) {
                    ids.insert(id);
                }
                if rec.geom_kind == geors_core::storage::GeomKind::Lines
                    && part.lines(&rec).iter().flatten().any(|&v| differs(v))
                {
                    line_ids.insert(id);
                }
            }
        }
        info!(regions = region_count, candidates = ids.len(), lines = line_ids.len(), elapsed = ?t.elapsed(), "patch: changed areas scanned");
        // Places within reach of changed place=* nodes. Lines get their
        // parents from their segments: rebuilt if the fallback differs at
        // any vertex.
        for (center, radius) in patched.locality_changes() {
            let within = |p: LonLat| geors_core::geom::haversine(p, center) <= radius;
            for id in part.search_bbox(&BBox::around(center, radius))? {
                let Some(rec) = part.record(id) else { continue };
                if rec.geom_kind != geors_core::storage::GeomKind::Lines {
                    if within(rec.center()) {
                        ids.insert(id);
                    }
                } else if !line_ids.contains(&id)
                    && part
                        .lines(&rec)
                        .iter()
                        .flatten()
                        .any(|&v| within(v) && kept.fallback_at(v) != patched.fallback_at(v))
                {
                    line_ids.insert(id);
                }
            }
        }
        candidates.extend(ids.into_iter().map(|id| (pi, id)));
        lines.extend(line_ids.into_iter().map(|id| (pi, id)));
    }

    info!(candidates = candidates.len(), lines = lines.len(), elapsed = ?t.elapsed(), "patch: changed place nodes scanned");
    // Full comparison for the candidates, on all cores.
    let names = |h: &Hierarchy, p: &geors_core::Place| -> Vec<(Layer, String)> {
        p.parents
            .iter()
            .filter_map(|&u| h.units.get(u as usize))
            .map(|u| (u.layer, u.name.clone()))
            .collect()
    };
    let mut out = HashSet::new();
    for (pi, id) in lines {
        let part = &parts[pi];
        if let Some(rec) = part.record(id) {
            out.extend(place_ids(&part.doc(&rec, false)?));
        }
    }
    let differing: Vec<Vec<(OsmType, i64)>> = candidates
        .par_iter()
        .map_init(
            || (HierCache::default(), HierCache::default()),
            |(a_cache, b_cache), &(pi, id)| -> Result<Vec<(OsmType, i64)>> {
                let part = &parts[pi];
                let Some(rec) = part.record(id) else {
                    return Ok(Vec::new());
                };
                let place = part.doc(&rec, false)?;
                let mut a = place.clone();
                a.parents.clear();
                a.postcode = None;
                a.country_code = None;
                let mut b = a.clone();
                kept.assign(a_cache, &mut a, kept.owner(place.osm_type, place.osm_id));
                patched.assign(b_cache, &mut b, patched.owner(place.osm_type, place.osm_id));
                let differs = names(kept, &a) != names(patched, &b)
                    || a.postcode != b.postcode
                    || a.country_code != b.country_code;
                Ok(if differs {
                    place_ids(&place).collect()
                } else {
                    Vec::new()
                })
            },
        )
        .collect::<Result<_>>()?;
    out.extend(differing.into_iter().flatten());
    Ok(out)
}

/// A sorted PBF of the given elements.
fn write_extract(
    path: &Path,
    nodes: &BTreeMap<i64, Elem>,
    ways: &BTreeMap<i64, Elem>,
    relations: &BTreeMap<i64, Elem>,
) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(&blob("OSMHeader", &header_block(None))?)?;
    for map in [nodes, ways, relations] {
        let elems: Vec<Elem> = map.values().cloned().collect();
        for chunk in elems.chunks(8000) {
            f.write_all(&blob("OSMData", &primitive_block(chunk))?)?;
        }
    }
    f.flush()?;
    Ok(())
}
