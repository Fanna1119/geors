//! Reads an OSM PBF file in three passes so that only the coordinates of nodes
//! that are actually needed are kept in memory:
//!
//! 1. relations: administrative boundaries and named multipolygons
//! 2. ways: features plus the member ways of relations from pass 1
//! 3. nodes: node features plus coordinates of nodes referenced in pass 2

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use geors_core::LonLat;
use osmpbf::{Element, ElementReader, RelMemberType};
use tracing::info;

use crate::tags::{self, Class, ElementKind, Tags};

pub struct NodeFeature {
    pub id: i64,
    pub point: LonLat,
    pub tags: Tags,
    pub class: Class,
}

pub struct WayData {
    pub id: i64,
    pub refs: Vec<i64>,
    /// Present when the way itself is a searchable feature.
    pub feature: Option<(Tags, Class)>,
}

pub struct RelationData {
    pub id: i64,
    pub tags: Tags,
    /// `None` for admin boundaries whose admin level we do not index as a
    /// feature but still use for the hierarchy.
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

/// Sorted node id -> coordinate lookup for the nodes we need.
pub struct NodeCoords {
    ids: Vec<i64>,
    coords: Vec<Option<LonLat>>,
}

impl NodeCoords {
    fn new(mut ids: Vec<i64>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        let coords = vec![None; ids.len()];
        Self { ids, coords }
    }

    fn set(&mut self, id: i64, p: LonLat) {
        if let Ok(i) = self.ids.binary_search(&id) {
            self.coords[i] = Some(p);
        }
    }

    pub fn get(&self, id: i64) -> Option<LonLat> {
        self.coords[self.ids.binary_search(&id).ok()?]
    }

    /// Resolve a node list, skipping nodes missing from the extract.
    pub fn line(&self, refs: &[i64]) -> Vec<LonLat> {
        refs.iter().filter_map(|&id| self.get(id)).collect()
    }
}

pub struct RawData {
    pub nodes: Vec<NodeFeature>,
    pub ways: Vec<WayData>,
    pub relations: Vec<RelationData>,
    pub coords: NodeCoords,
}

pub fn read_pbf(path: &Path) -> Result<RawData> {
    let open = || {
        ElementReader::from_path(path)
            .with_context(|| format!("cannot open PBF file '{}'", path.display()))
    };

    // Pass 1: relations.
    let t = Instant::now();
    let mut relations = Vec::new();
    open()?
        .for_each(|el| {
            let Element::Relation(r) = el else { return };
            if !tags::may_be_interesting(r.tags()) {
                return;
            }
            let t = Tags::from_osm(r.tags());
            if !matches!(t.get("type"), Some("multipolygon" | "boundary")) {
                return;
            }
            let class = tags::classify(&t, ElementKind::Relation);
            let admin = t.get("boundary") == Some("administrative")
                && t.admin_level().is_some()
                && t.name().is_some();
            let postal = t.get("boundary") == Some("postal_code") && t.has("postal_code");
            if class.is_none() && !admin && !postal {
                return;
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
            if !rel.outer.is_empty() {
                relations.push(rel);
            }
        })
        .with_context(|| format!("failed reading relations from '{}'", path.display()))?;
    let member_ways: HashSet<i64> = relations
        .iter()
        .flat_map(|r| r.outer.iter().chain(r.inner.iter()).copied())
        .collect();
    info!(relations = relations.len(), member_ways = member_ways.len(), elapsed = ?t.elapsed(), "pass 1/3: relations");

    // Pass 2: ways.
    let t = Instant::now();
    let mut ways = Vec::new();
    open()?
        .for_each(|el| {
            let Element::Way(w) = el else { return };
            let feature = if tags::may_be_interesting(w.tags()) {
                let t = Tags::from_osm(w.tags());
                tags::classify(&t, ElementKind::Way).map(|c| (t, c))
            } else {
                None
            };
            if feature.is_some() || member_ways.contains(&w.id()) {
                ways.push(WayData {
                    id: w.id(),
                    refs: w.refs().collect(),
                    feature,
                });
            }
        })
        .with_context(|| format!("failed reading ways from '{}'", path.display()))?;
    ways.sort_unstable_by_key(|w| w.id);
    let needed: Vec<i64> = ways
        .iter()
        .flat_map(|w| w.refs.iter().copied())
        .chain(
            relations
                .iter()
                .flat_map(|r| r.label.into_iter().chain(r.admin_centre)),
        )
        .collect();
    let mut coords = NodeCoords::new(needed);
    info!(ways = ways.len(), needed_nodes = coords.ids.len(), elapsed = ?t.elapsed(), "pass 2/3: ways");

    // Pass 3: nodes.
    let t = Instant::now();
    let mut nodes = Vec::new();
    open()?
        .for_each(|el| {
            let (id, point, tags) = match el {
                Element::DenseNode(n) => (
                    n.id(),
                    LonLat::new(n.lon(), n.lat()),
                    tags::may_be_interesting(n.tags()).then(|| Tags::from_osm(n.tags())),
                ),
                Element::Node(n) => (
                    n.id(),
                    LonLat::new(n.lon(), n.lat()),
                    tags::may_be_interesting(n.tags()).then(|| Tags::from_osm(n.tags())),
                ),
                _ => return,
            };
            coords.set(id, point);
            if let Some(tags) = tags
                && let Some(class) = tags::classify(&tags, ElementKind::Node)
            {
                nodes.push(NodeFeature {
                    id,
                    point,
                    tags,
                    class,
                });
            }
        })
        .with_context(|| format!("failed reading nodes from '{}'", path.display()))?;
    let missing = coords.coords.iter().filter(|c| c.is_none()).count();
    info!(node_features = nodes.len(), missing_nodes = missing, elapsed = ?t.elapsed(), "pass 3/3: nodes");

    Ok(RawData {
        nodes,
        ways,
        relations,
        coords,
    })
}
