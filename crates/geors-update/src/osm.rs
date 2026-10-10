//! Owned OSM elements and change sets, used to apply replication diffs to
//! a PBF file.

use std::collections::BTreeMap;

pub type Tags = Vec<(String, String)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Node = 0,
    Way = 1,
    Relation = 2,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub kind: Kind,
    pub id: i64,
    pub role: String,
}

/// An OSM element without metadata (version, user, ...): the geocoder does
/// not need it, and diffs are applied by id.
#[derive(Debug, Clone, PartialEq)]
pub enum Elem {
    Node {
        id: i64,
        /// Fixed point, 1e-7 degrees (as in PBF with granularity 100).
        lon: i32,
        lat: i32,
        tags: Tags,
    },
    Way {
        id: i64,
        refs: Vec<i64>,
        tags: Tags,
    },
    Relation {
        id: i64,
        members: Vec<Member>,
        tags: Tags,
    },
}

impl Elem {
    pub fn kind(&self) -> Kind {
        match self {
            Elem::Node { .. } => Kind::Node,
            Elem::Way { .. } => Kind::Way,
            Elem::Relation { .. } => Kind::Relation,
        }
    }

    pub fn id(&self) -> i64 {
        match self {
            Elem::Node { id, .. } | Elem::Way { id, .. } | Elem::Relation { id, .. } => *id,
        }
    }

    /// Owned copy of an element read with `osmpbf`.
    pub fn from_pbf(el: &osmpbf::Element) -> Option<Elem> {
        let tags = |it: &mut dyn Iterator<Item = (&str, &str)>| -> Tags {
            it.map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        Some(match el {
            osmpbf::Element::Node(n) => Elem::Node {
                id: n.id(),
                lon: n.decimicro_lon(),
                lat: n.decimicro_lat(),
                tags: tags(&mut n.tags()),
            },
            osmpbf::Element::DenseNode(n) => Elem::Node {
                id: n.id(),
                lon: n.decimicro_lon(),
                lat: n.decimicro_lat(),
                tags: tags(&mut n.tags()),
            },
            osmpbf::Element::Way(w) => Elem::Way {
                id: w.id(),
                refs: w.refs().collect(),
                tags: tags(&mut w.tags()),
            },
            osmpbf::Element::Relation(r) => Elem::Relation {
                id: r.id(),
                members: r
                    .members()
                    .map(|m| Member {
                        kind: match m.member_type {
                            osmpbf::RelMemberType::Node => Kind::Node,
                            osmpbf::RelMemberType::Way => Kind::Way,
                            osmpbf::RelMemberType::Relation => Kind::Relation,
                        },
                        id: m.member_id,
                        role: m.role().unwrap_or("").to_string(),
                    })
                    .collect(),
                tags: tags(&mut r.tags()),
            },
        })
    }
}

/// Net effect of one or more diffs: per element, its new version or `None`
/// for a deletion. Later diffs override earlier ones.
#[derive(Debug, Default)]
pub struct ChangeSet {
    pub nodes: BTreeMap<i64, Option<Elem>>,
    pub ways: BTreeMap<i64, Option<Elem>>,
    pub relations: BTreeMap<i64, Option<Elem>>,
}

impl ChangeSet {
    pub fn map(&mut self, kind: Kind) -> &mut BTreeMap<i64, Option<Elem>> {
        match kind {
            Kind::Node => &mut self.nodes,
            Kind::Way => &mut self.ways,
            Kind::Relation => &mut self.relations,
        }
    }

    pub fn upsert(&mut self, e: Elem) {
        self.map(e.kind()).insert(e.id(), Some(e));
    }

    pub fn delete(&mut self, kind: Kind, id: i64) {
        self.map(kind).insert(id, None);
    }

    pub fn len(&self) -> usize {
        self.nodes.len() + self.ways.len() + self.relations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
