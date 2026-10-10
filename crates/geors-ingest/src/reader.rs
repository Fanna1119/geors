//! Parallel streaming passes over an OSM PBF file.
//!
//! The import makes five passes so that nothing proportional to the number
//! of features has to stay in memory:
//!
//! 1. [`read_relations`]: boundaries and named multipolygons (kept: few).
//!    Also records which blobs hold nodes, ways and relations, so later
//!    passes only decompress the blobs they need.
//! 2. [`scan_ways`]: ids of nodes needed by interesting ways; node lists of
//!    relation member ways and `place=*` area ways (kept: few)
//! 3. [`scan_nodes`]: coordinates of the needed nodes (disk-backed), plus
//!    `place=*` nodes for the locality fallback
//! 4. [`stream_ways`]: way features
//! 5. [`stream_nodes`]: node features
//!
//! Blobs are decoded and mapped on all cores (rayon); results flow through
//! a bounded channel to one consumer on the calling thread, which does the
//! order-sensitive writes. Block order is therefore not preserved; callers
//! sort where it matters.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use geors_core::LonLat;
use geors_core::geom::from_e7;
use osmpbf::{BlobDecode, BlobReader, ByteOffset, Element, PrimitiveBlock, RelMemberType};
use rayon::prelude::*;
use tracing::info;

use crate::join::{Joiner, RefSink};
use crate::nodes::{CoordArray, IdSink, NodeCoords, NodeIds};
use crate::tags::{self, Class, ElementKind, Tags};

pub struct RelationData {
    pub id: i64,
    pub tags: Tags,
    /// `None` for boundaries only used for the hierarchy / postcodes.
    pub class: Option<Class>,
    pub outer: Vec<i64>,
    pub inner: Vec<i64>,
    pub label: Option<i64>,
    pub admin_centre: Option<i64>,
}

impl RelationData {
    /// `postal_code` of a `boundary=postal_code` relation.
    pub fn postal_code(&self) -> Option<&str> {
        (self.tags.get("boundary") == Some("postal_code"))
            .then(|| self.tags.get("postal_code"))
            .flatten()
            .map(str::trim)
            .filter(|c| !c.is_empty())
    }

    pub fn is_admin_boundary(&self) -> bool {
        self.tags.get("boundary") == Some("administrative") && self.tags.admin_level().is_some()
    }
}

/// Offsets of the blobs holding each element type (from pass 1).
pub struct BlobIndex {
    path: PathBuf,
    nodes: Vec<ByteOffset>,
    ways: Vec<ByteOffset>,
}

/// Run `map` for every block on the rayon pool (with per-worker state from
/// `init`) and feed results to `consume` on the calling thread. The first
/// error stops further work. At most two results per thread are in flight.
fn drive<P, B, S, T>(
    blocks: P,
    init: impl Fn() -> S + Sync + Send,
    map: impl Fn(&mut S, B) -> Result<T> + Sync + Send,
    mut consume: impl FnMut(T) -> Result<()>,
) -> Result<()>
where
    P: ParallelIterator<Item = Result<Option<B>>>,
    T: Send,
{
    let (tx, rx) = sync_channel::<Result<T>>(2 * rayon::current_num_threads());
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (stop, init, map) = (&stop, &init, &map);
        let producer = scope.spawn(move || {
            blocks.for_each_init(
                || (tx.clone(), init()),
                |(tx, state), block| {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let r = match block {
                        Ok(Some(b)) => map(state, b),
                        Ok(None) => return,
                        Err(e) => Err(e),
                    };
                    if r.is_err() || tx.send(r).is_err() {
                        stop.store(true, Ordering::Relaxed);
                    }
                },
            );
        });
        let mut result = Ok(());
        for r in rx.iter() {
            if let Err(e) = r.and_then(&mut consume) {
                result = Err(e);
                stop.store(true, Ordering::Relaxed);
                break;
            }
        }
        drop(rx); // unblocks producers waiting on a full channel
        producer
            .join()
            .map_err(|_| anyhow!("PBF decoder thread panicked"))?;
        result
    })
}

fn open_seekable(path: &Path) -> Result<BlobReader<BufReader<File>>> {
    BlobReader::seekable_from_path(path)
        .with_context(|| format!("cannot open PBF file '{}'", path.display()))
}

fn to_block(blob: osmpbf::Result<osmpbf::Blob>) -> Result<Option<PrimitiveBlock>> {
    match blob?.decode()? {
        BlobDecode::OsmData(b) => Ok(Some(b)),
        _ => Ok(None),
    }
}

/// Decode only the blobs at `offsets`, each worker with its own file handle.
fn drive_offsets<S, T>(
    path: &Path,
    offsets: &[ByteOffset],
    init: impl Fn() -> S + Sync + Send,
    map: impl Fn(&mut S, PrimitiveBlock) -> Result<T> + Sync + Send,
    consume: impl FnMut(T) -> Result<()>,
) -> Result<()>
where
    T: Send,
{
    let blocks = offsets.par_iter().map_init(
        || open_seekable(path),
        |reader, &offset| {
            let reader = reader.as_mut().map_err(|e| anyhow!("{e:#}"))?;
            Ok(Some(reader.blob_from_offset(offset)?.to_primitiveblock()?))
        },
    );
    drive(blocks, init, map, consume)
}

fn relation_data(r: &osmpbf::Relation) -> Option<RelationData> {
    if !tags::may_be_interesting(r.tags()) {
        return None;
    }
    let t = Tags::from_osm(r.tags());
    if !matches!(t.get("type"), Some("multipolygon" | "boundary")) {
        return None;
    }
    let class = tags::classify(&t, ElementKind::Relation);
    let admin = t.get("boundary") == Some("administrative")
        && t.admin_level().is_some()
        && t.name().is_some();
    let postal = t.get("boundary") == Some("postal_code") && t.has("postal_code");
    if class.is_none() && !admin && !postal {
        return None;
    }
    let mut rel = RelationData {
        id: r.id(),
        tags: t,
        class,
        outer: Vec::new(),
        inner: Vec::new(),
        label: None,
        admin_centre: None,
    };
    for m in r.members() {
        let role = m.role().unwrap_or("");
        match (m.member_type, role) {
            (RelMemberType::Way, "inner") => rel.inner.push(m.member_id),
            (RelMemberType::Way, "outer" | "") => rel.outer.push(m.member_id),
            (RelMemberType::Node, "label") => rel.label = Some(m.member_id),
            (RelMemberType::Node, "admin_centre") => rel.admin_centre = Some(m.member_id),
            _ => {}
        }
    }
    (!rel.outer.is_empty()).then_some(rel)
}

/// Pass 1: relations, plus the blob index for the later passes.
pub fn read_relations(path: &Path) -> Result<(BlobIndex, Vec<RelationData>)> {
    let t = Instant::now();
    let blocks = open_seekable(path)?.par_bridge().map(|blob| {
        let blob = blob?;
        let offset = blob
            .offset()
            .ok_or_else(|| anyhow!("blob without offset"))?;
        Ok(to_block(Ok(blob))?.map(|b| (offset, b)))
    });
    let mut index = BlobIndex {
        path: path.to_path_buf(),
        nodes: Vec::new(),
        ways: Vec::new(),
    };
    let mut relations = Vec::new();
    drive(
        blocks,
        || (),
        |_, (offset, block): (ByteOffset, PrimitiveBlock)| {
            let (mut has_nodes, mut has_ways, mut rels) = (false, false, Vec::new());
            for g in block.groups() {
                has_nodes |= g.nodes().next().is_some() || g.dense_nodes().next().is_some();
                has_ways |= g.ways().next().is_some();
                rels.extend(g.relations().filter_map(|r| relation_data(&r)));
            }
            Ok((offset, has_nodes, has_ways, rels))
        },
        |(offset, has_nodes, has_ways, rels)| {
            if has_nodes {
                index.nodes.push(offset);
            }
            if has_ways {
                index.ways.push(offset);
            }
            relations.extend(rels);
            Ok(())
        },
    )
    .with_context(|| format!("failed reading '{}'", path.display()))?;
    // Deterministic order regardless of which thread finished first.
    index.nodes.sort_by_key(|o| o.0);
    index.ways.sort_by_key(|o| o.0);
    relations.sort_by_key(|r| r.id);
    info!(
        relations = relations.len(),
        node_blobs = index.nodes.len(),
        way_blobs = index.ways.len(),
        elapsed = ?t.elapsed(),
        "pass 1/5: relations"
    );
    Ok((index, relations))
}

/// A closed way tagged `place=*` that acts as an area in the hierarchy.
pub struct AreaWay {
    pub id: i64,
    pub refs: Vec<i64>,
    pub tags: Tags,
    pub class: Class,
}

pub struct WayScan {
    /// Node lists of ways that are members of relations from pass 1.
    pub member_refs: HashMap<i64, Vec<i64>>,
    pub area_ways: Vec<AreaWay>,
    pub feature_ways: u64,
}

pub fn is_closed(refs: &[i64]) -> bool {
    refs.len() >= 4 && refs.first() == refs.last()
}

#[derive(Default)]
struct WayBlock {
    ids: Vec<i64>,
    /// Sorted-join mode: (way id, node refs) of feature ways.
    joined: Vec<(i64, Vec<i64>)>,
    members: Vec<(i64, Vec<i64>)>,
    areas: Vec<AreaWay>,
    features: u64,
}

/// Pass 2: record which nodes are needed (into `ids`).
/// With `join` (sorted-join mode), feature ways' node references go to the
/// join instead of the random-access store; only boundary member ways and
/// `place=*` area ways (needed before pass 4) still use the store.
pub fn scan_ways(
    index: &BlobIndex,
    member_ways: &HashSet<i64>,
    ids: &mut IdSink,
    mut join: Option<&mut RefSink>,
) -> Result<WayScan> {
    let sorted = join.is_some();
    let t = Instant::now();
    let mut scan = WayScan {
        member_refs: HashMap::new(),
        area_ways: Vec::new(),
        feature_ways: 0,
    };
    drive_offsets(
        &index.path,
        &index.ways,
        || (),
        |_, block| {
            let mut out = WayBlock::default();
            for el in block.elements() {
                let Element::Way(w) = el else { continue };
                let member = member_ways.contains(&w.id());
                let feature = if tags::may_be_interesting(w.tags()) {
                    let t = Tags::from_osm(w.tags());
                    tags::classify(&t, ElementKind::Way).map(|c| (t, c))
                } else {
                    None
                };
                if !member && feature.is_none() {
                    continue;
                }
                let refs: Vec<i64> = w.refs().collect();
                let area = feature.as_ref().is_some_and(|(_, c)| {
                    c.key == "place" && c.layer.is_admin() && is_closed(&refs)
                });
                if !sorted || member || area {
                    out.ids.extend_from_slice(&refs);
                }
                if sorted && feature.is_some() {
                    out.joined.push((w.id(), refs.clone()));
                }
                if let Some((tags, class)) = feature {
                    out.features += 1;
                    if area {
                        out.areas.push(AreaWay {
                            id: w.id(),
                            refs: refs.clone(),
                            tags,
                            class,
                        });
                    }
                }
                if member {
                    out.members.push((w.id(), refs));
                }
            }
            Ok(out)
        },
        |b| {
            ids.extend(b.ids)?;
            if let Some(join) = join.as_deref_mut() {
                for (way, refs) in &b.joined {
                    join.push_way(*way, refs)?;
                }
            }
            scan.member_refs.extend(b.members);
            scan.area_ways.extend(b.areas);
            scan.feature_ways += b.features;
            Ok(())
        },
    )?;
    scan.area_ways.sort_by_key(|w| w.id);
    info!(
        feature_ways = scan.feature_ways,
        member_ways = scan.member_refs.len(),
        elapsed = ?t.elapsed(),
        "pass 2/5: ways"
    );
    Ok(scan)
}

/// Id, position (fixed point, exactly as in the PBF and in [`NodeCoords`],
/// so a node and a polygon vertex at the same spot compare equal) and
/// whether the node may carry interesting tags.
fn node_parts(el: &Element) -> Option<(i64, i32, i32, bool)> {
    match el {
        Element::DenseNode(n) => Some((
            n.id(),
            n.decimicro_lon(),
            n.decimicro_lat(),
            tags::may_be_interesting(n.tags()),
        )),
        Element::Node(n) => Some((
            n.id(),
            n.decimicro_lon(),
            n.decimicro_lat(),
            tags::may_be_interesting(n.tags()),
        )),
        _ => None,
    }
}

fn node_tags(el: &Element) -> Tags {
    match el {
        Element::DenseNode(n) => Tags::from_osm(n.tags()),
        Element::Node(n) => Tags::from_osm(n.tags()),
        _ => Tags::default(),
    }
}

/// A node tagged `place=*` (city, village, suburb, ...).
pub struct PlaceNode {
    pub id: i64,
    pub point: LonLat,
    pub tags: Tags,
    pub class: Class,
}

/// Pass 3: fill `coords`; return the `place=*` nodes, sorted by id.
pub fn scan_nodes(index: &BlobIndex, coords: &mut NodeCoords) -> Result<Vec<PlaceNode>> {
    let t = Instant::now();
    let mut places = Vec::new();
    let (ids, values): (&NodeIds, &mut CoordArray) = coords.parts_mut();
    drive_offsets(
        &index.path,
        &index.nodes,
        || (),
        |_, block| {
            let mut found: Vec<(u32, i32, i32)> = Vec::new();
            let mut place_nodes = Vec::new();
            for el in block.elements() {
                let Some((id, lon, lat, interesting)) = node_parts(&el) else {
                    continue;
                };
                if let Some(i) = ids.index(id) {
                    found.push((i as u32, lon, lat));
                }
                if interesting {
                    let tags = node_tags(&el);
                    if tags.has("place")
                        && let Some(class) = tags::classify(&tags, ElementKind::Node)
                        && class.key == "place"
                    {
                        let point = LonLat::new(from_e7(lon), from_e7(lat));
                        place_nodes.push(PlaceNode {
                            id,
                            point,
                            tags,
                            class,
                        });
                    }
                }
            }
            Ok((found, place_nodes))
        },
        |(found, place_nodes)| {
            for (i, lon, lat) in found {
                values.set(i as usize, lon, lat);
            }
            places.extend(place_nodes);
            Ok(())
        },
    )?;
    places.sort_by_key(|p: &PlaceNode| p.id);
    info!(
        needed_nodes = coords.len(),
        missing_nodes = coords.missing(),
        place_nodes = places.len(),
        elapsed = ?t.elapsed(),
        "pass 3/5: node coordinates"
    );
    Ok(places)
}

/// Pass 3 in sorted-join mode: nodes are consumed in id order (blobs are
/// decoded in parallel batches, then processed in file order) so the
/// joiner can sweep the sorted node references alongside them.
pub fn scan_nodes_joined(
    index: &BlobIndex,
    coords: &mut NodeCoords,
    joiner: &mut Joiner,
) -> Result<Vec<PlaceNode>> {
    const BATCH: usize = 64;
    let t = Instant::now();
    let mut places = Vec::new();
    let (ids, values) = coords.parts_mut();
    type Decoded = (Vec<(i64, i32, i32)>, Vec<PlaceNode>);
    for batch in index.nodes.chunks(BATCH) {
        let decoded: Vec<Result<Decoded>> = batch
            .par_iter()
            .map_init(
                || open_seekable(&index.path),
                |reader, &offset| {
                    let reader = reader.as_mut().map_err(|e| anyhow!("{e:#}"))?;
                    let block = reader.blob_from_offset(offset)?.to_primitiveblock()?;
                    let mut nodes = Vec::new();
                    let mut place_nodes = Vec::new();
                    for el in block.elements() {
                        let Some((id, lon, lat, interesting)) = node_parts(&el) else {
                            continue;
                        };
                        nodes.push((id, lon, lat));
                        if interesting {
                            let tags = node_tags(&el);
                            if tags.has("place")
                                && let Some(class) = tags::classify(&tags, ElementKind::Node)
                                && class.key == "place"
                            {
                                let point = LonLat::new(from_e7(lon), from_e7(lat));
                                place_nodes.push(PlaceNode { id, point, tags, class });
                            }
                        }
                    }
                    Ok((nodes, place_nodes))
                },
            )
            .collect();
        for d in decoded {
            let (nodes, place_nodes) = d?;
            for (id, lon, lat) in nodes {
                if let Some(i) = ids.index(id) {
                    values.set(i, lon, lat);
                }
                joiner.node(id, lon, lat)?;
            }
            places.extend(place_nodes);
        }
    }
    places.sort_by_key(|p: &PlaceNode| p.id);
    info!(
        place_nodes = places.len(),
        elapsed = ?t.elapsed(),
        "pass 3/5: node coordinates (sorted join)"
    );
    Ok(places)
}

/// Pass 4: `map` every way feature on a worker (with per-worker state),
/// `consume` the results on the calling thread.
pub fn stream_ways<S, T: Send>(
    index: &BlobIndex,
    init: impl Fn() -> S + Sync + Send,
    map: impl Fn(&mut S, i64, &[i64], Tags, Class) -> Result<Option<T>> + Sync + Send,
    mut consume: impl FnMut(T) -> Result<()>,
) -> Result<()> {
    let t = Instant::now();
    let mut n = 0u64;
    drive_offsets(
        &index.path,
        &index.ways,
        || (init(), Vec::new()),
        |(state, refs), block| {
            let mut out = Vec::new();
            for el in block.elements() {
                let Element::Way(w) = el else { continue };
                if !tags::may_be_interesting(w.tags()) {
                    continue;
                }
                let tags = Tags::from_osm(w.tags());
                let Some(class) = tags::classify(&tags, ElementKind::Way) else {
                    continue;
                };
                refs.clear();
                refs.extend(w.refs());
                if let Some(v) = map(state, w.id(), refs, tags, class)? {
                    out.push(v);
                }
            }
            Ok(out)
        },
        |items| {
            n += items.len() as u64;
            items.into_iter().try_for_each(&mut consume)
        },
    )?;
    info!(ways = n, elapsed = ?t.elapsed(), "pass 4/5: way features");
    Ok(())
}

/// Pass 5: like [`stream_ways`], for node features.
pub fn stream_nodes<S, T: Send>(
    index: &BlobIndex,
    init: impl Fn() -> S + Sync + Send,
    map: impl Fn(&mut S, i64, LonLat, Tags, Class) -> Result<Option<T>> + Sync + Send,
    mut consume: impl FnMut(T) -> Result<()>,
) -> Result<()> {
    let t = Instant::now();
    let mut n = 0u64;
    drive_offsets(
        &index.path,
        &index.nodes,
        init,
        |state, block| {
            let mut out = Vec::new();
            for el in block.elements() {
                let Some((id, lon, lat, true)) = node_parts(&el) else {
                    continue;
                };
                let tags = node_tags(&el);
                let Some(class) = tags::classify(&tags, ElementKind::Node) else {
                    continue;
                };
                let point = LonLat::new(from_e7(lon), from_e7(lat));
                if let Some(v) = map(state, id, point, tags, class)? {
                    out.push(v);
                }
            }
            Ok(out)
        },
        |items| {
            n += items.len() as u64;
            items.into_iter().try_for_each(&mut consume)
        },
    )?;
    info!(nodes = n, elapsed = ?t.elapsed(), "pass 5/5: node features");
    Ok(())
}
