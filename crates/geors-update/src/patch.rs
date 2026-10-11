//! Incremental updates: turn the changes since a base PBF into patch
//! partitions, without re-importing the extract.
//!
//! The affected places are rebuilt by the normal importer: their elements,
//! and everything those reference, are written to a small PBF that is
//! imported with the kept address hierarchy (see `docs/incremental-updates.md`).
//! The patch is always built from all changes since the base, so it never
//! depends on an earlier patch.

use std::collections::{BTreeMap, BTreeSet, HashSet};
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

/// Build patch partitions in `data_dir` for all `changes` since `base`.
pub fn build_patch(
    base: &Path,
    changes: &ChangeSet,
    hierarchy: Arc<Hierarchy>,
    data_dir: &Path,
    work: &Path,
) -> Result<PatchStats> {
    let t = Instant::now();
    std::fs::create_dir_all(work)?;
    let index = BlockIndex::load_or_build(base)?;
    let source = ElementSource::new(base, &index, changes);
    let mut stats = PatchStats::default();

    // 1. Roots: changed elements, ways using a changed node, relations
    //    using a root way or changed node.
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
    info!(ways = roots.ways.len(), relations = roots.relations.len(), elapsed = ?t.elapsed(), "patch: roots found");

    // 2. Current versions of the root ways and relations; the label nodes
    //    of root relations become roots too (a boundary's label node takes
    //    over its polygon).
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

    // 3. Street completion: a changed segment is merged with the other
    //    segments of its street, so all of them are rebuilt. Old merged
    //    places that contain a root way, and same-named merged places close
    //    to a changed street segment.
    let parts: Vec<Partition> = geors_index::engine::list_partitions(data_dir)?
        .into_iter()
        .map(|cc| Partition::open(&data_dir.join(cc)).map_err(anyhow::Error::from))
        .collect::<Result<_>>()?;
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
    for part in &parts {
        let mut add = |p: &geors_core::Place| {
            if p.layer == Layer::Street || p.osm_key == "waterway" {
                extra.extend(place_ids(p).map(|(_, id)| id));
            }
        };
        // Old merged places of root ways.
        for id in part.text.lookup(&root_keys)? {
            if let Some(rec) = part.record(id) {
                add(&part.doc(&rec, false)?);
            }
        }
        // Same-named merged places near changed street segments.
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
    stats.roots = roots.len();

    // 4. Everything the roots reference: member ways of relations, and the
    //    nodes of all ways.
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
    stats.elements = nodes.len() + ways.len() + relations.len();

    // 5. The small extract, imported with the kept hierarchy.
    let extract = work.join("patch-extract.osm.pbf");
    write_extract(&extract, &nodes, &ways, &relations)?;
    drop((nodes, ways, relations));
    let opts = ImportOptions {
        all_countries: true,
        work_dir: Some(work.to_path_buf()),
        node_lookup: NodeLookup::Memory,
        hierarchy: Some(hierarchy),
        ..Default::default()
    };
    let import = geors_ingest::import_pbf(&extract, &opts)?;

    // 6. Per country: the rebuilt places of roots, and tombstones for the
    //    old places of roots and of everything rebuilt.
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
