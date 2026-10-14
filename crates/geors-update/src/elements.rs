//! OSM elements by id, from a base PBF plus the changes since.
//!
//! For incremental updates the base PBF stays as it is between full
//! rebuilds; diffs accumulate in a [`ChangeSet`] (the change store) instead
//! of being merged into the base, which rewrites the whole file (Germany:
//! 74 s per diff). Reading an element asks the changes first, then the
//! base. A [`BlockIndex`] (first and last id of every block) lets a sorted
//! batch of ids decode only the blocks that hold them.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use osmpbf::{BlobDecode, BlobReader, ByteOffset};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::osm::{ChangeSet, Elem, Kind};

/// One data block of a PBF file.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Block {
    kind: Kind,
    first: i64,
    last: i64,
    offset: u64,
}

/// First and last id of every block of a PBF, by kind. Assumes, as for
/// Geofabrik and planet files, that each block holds one kind of element,
/// sorted by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockIndex {
    /// Size and modification time of the indexed file (staleness check).
    file_len: u64,
    file_mtime: u64,
    blocks: Vec<Block>,
}

fn stamp(path: &Path) -> Result<(u64, u64)> {
    let m = std::fs::metadata(path).with_context(|| format!("cannot read '{}'", path.display()))?;
    let mtime = m
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok((m.len(), mtime))
}

impl BlockIndex {
    /// The index of `pbf` saved at `path` if it is current, else build and
    /// save it there.
    pub fn load_or_build(pbf: &Path, path: &Path) -> Result<Self> {
        let (len, mtime) = stamp(pbf)?;
        if let Ok(bytes) = std::fs::read(path)
            && let Ok(index) = postcard::from_bytes::<BlockIndex>(&bytes)
            && index.file_len == len
            && index.file_mtime == mtime
        {
            return Ok(index);
        }
        let index = Self::build(pbf)?;
        std::fs::write(path, postcard::to_allocvec(&index)?)
            .with_context(|| format!("cannot write '{}'", path.display()))?;
        Ok(index)
    }

    /// One pass over the file, decoding blocks on all cores.
    pub fn build(pbf: &Path) -> Result<Self> {
        let t = Instant::now();
        let (file_len, file_mtime) = stamp(pbf)?;
        let reader = BlobReader::seekable_from_path(pbf)
            .with_context(|| format!("cannot open '{}'", pbf.display()))?;
        let mut blocks: Vec<Block> = reader
            .par_bridge()
            .map(|blob| -> Result<Option<Block>> {
                let blob = blob?;
                let offset = blob.offset().map(|o| o.0).context("blob without offset")?;
                let BlobDecode::OsmData(block) = blob.decode()? else {
                    return Ok(None);
                };
                let mut range: Option<(Kind, i64, i64)> = None;
                for el in block.elements() {
                    let (kind, id) = match el {
                        osmpbf::Element::Node(n) => (Kind::Node, n.id()),
                        osmpbf::Element::DenseNode(n) => (Kind::Node, n.id()),
                        osmpbf::Element::Way(w) => (Kind::Way, w.id()),
                        osmpbf::Element::Relation(r) => (Kind::Relation, r.id()),
                    };
                    range = Some(match range {
                        None => (kind, id, id),
                        Some((k, lo, hi)) => {
                            if k != kind {
                                bail!("block at offset {offset} mixes element kinds");
                            }
                            (k, lo.min(id), hi.max(id))
                        }
                    });
                }
                Ok(range.map(|(kind, first, last)| Block {
                    kind,
                    first,
                    last,
                    offset,
                }))
            })
            .filter_map(|r| r.transpose())
            .collect::<Result<_>>()?;
        blocks.sort_by_key(|b| (b.kind, b.first, b.offset));
        info!(blocks = blocks.len(), elapsed = ?t.elapsed(), "block index built");
        Ok(Self {
            file_len,
            file_mtime,
            blocks,
        })
    }

    /// Blocks of `kind` that may hold `id`.
    fn blocks_for(&self, kind: Kind, id: i64) -> impl Iterator<Item = &Block> {
        let start = self
            .blocks
            .partition_point(|b| (b.kind, b.first) < (kind, id));
        // A block starting before `id` may still contain it, if it is of the
        // same kind (the first block of a kind follows one of another kind).
        let from = match start.checked_sub(1) {
            Some(prev) if self.blocks[prev].kind == kind => prev,
            _ => start,
        };
        self.blocks[from..]
            .iter()
            .take_while(move |b| b.kind == kind && b.first <= id)
            .filter(move |b| b.last >= id)
    }
}

/// Elements by id from a base PBF and the changes since.
pub struct ElementSource<'a> {
    pbf: PathBuf,
    index: &'a BlockIndex,
    changes: &'a ChangeSet,
}

impl<'a> ElementSource<'a> {
    pub fn new(pbf: &Path, index: &'a BlockIndex, changes: &'a ChangeSet) -> Self {
        Self {
            pbf: pbf.to_path_buf(),
            index,
            changes,
        }
    }

    /// The current version of each element in `ids` (missing or deleted
    /// ones are absent).
    pub fn get(&self, kind: Kind, ids: &[i64]) -> Result<BTreeMap<i64, Elem>> {
        let changed = match kind {
            Kind::Node => &self.changes.nodes,
            Kind::Way => &self.changes.ways,
            Kind::Relation => &self.changes.relations,
        };
        let mut out = BTreeMap::new();
        let mut from_base = Vec::new();
        for &id in ids {
            match changed.get(&id) {
                Some(Some(e)) => {
                    out.insert(id, e.clone());
                }
                Some(None) => {} // deleted
                None => from_base.push(id),
            }
        }
        from_base.sort_unstable();
        from_base.dedup();
        out.extend(self.read_base(kind, &from_base)?);
        Ok(out)
    }

    /// Elements of the base only, decoding just the blocks that hold them.
    fn read_base(&self, kind: Kind, ids: &[i64]) -> Result<BTreeMap<i64, Elem>> {
        // ids (sorted) per block offset.
        let mut wanted: BTreeMap<u64, Vec<i64>> = BTreeMap::new();
        for &id in ids {
            for b in self.index.blocks_for(kind, id) {
                wanted.entry(b.offset).or_default().push(id);
            }
        }
        let found: Vec<Vec<Elem>> = wanted
            .into_par_iter()
            .map_init(
                || BlobReader::seekable_from_path(&self.pbf),
                |reader, (offset, ids)| -> Result<Vec<Elem>> {
                    let reader = reader.as_mut().map_err(|e| anyhow::anyhow!("{e}"))?;
                    let block = reader
                        .blob_from_offset(ByteOffset(offset))?
                        .to_primitiveblock()?;
                    let mut out = Vec::new();
                    for el in block.elements() {
                        let id = match &el {
                            osmpbf::Element::Node(n) => n.id(),
                            osmpbf::Element::DenseNode(n) => n.id(),
                            osmpbf::Element::Way(w) => w.id(),
                            osmpbf::Element::Relation(r) => r.id(),
                        };
                        if ids.binary_search(&id).is_ok()
                            && let Some(e) = Elem::from_pbf(&el)
                            && e.kind() == kind
                        {
                            out.push(e);
                        }
                    }
                    Ok(out)
                },
            )
            .collect::<Result<_>>()?;
        Ok(found.into_iter().flatten().map(|e| (e.id(), e)).collect())
    }
}

impl ElementSource<'_> {
    /// Ids of current elements of `kind` for which `keep` holds: one
    /// parallel pass over the base's blocks of that kind (changed elements
    /// are judged by their new version, deleted ones skipped).
    pub fn scan(
        &self,
        kind: Kind,
        keep: impl Fn(&Elem) -> bool + Sync,
    ) -> Result<std::collections::BTreeSet<i64>> {
        let changed = match kind {
            Kind::Node => &self.changes.nodes,
            Kind::Way => &self.changes.ways,
            Kind::Relation => &self.changes.relations,
        };
        let offsets: Vec<u64> = self
            .index
            .blocks
            .iter()
            .filter(|b| b.kind == kind)
            .map(|b| b.offset)
            .collect();
        let found: Vec<Vec<i64>> = offsets
            .into_par_iter()
            .map_init(
                || BlobReader::seekable_from_path(&self.pbf),
                |reader, offset| -> Result<Vec<i64>> {
                    let reader = reader.as_mut().map_err(|e| anyhow::anyhow!("{e}"))?;
                    let block = reader
                        .blob_from_offset(ByteOffset(offset))?
                        .to_primitiveblock()?;
                    Ok(block
                        .elements()
                        .filter_map(|el| Elem::from_pbf(&el))
                        .filter(|e| !changed.contains_key(&e.id()) && keep(e))
                        .map(|e| e.id())
                        .collect())
                },
            )
            .collect::<Result<_>>()?;
        let mut out: std::collections::BTreeSet<i64> = found.into_iter().flatten().collect();
        out.extend(changed.values().flatten().filter(|e| keep(e)).map(Elem::id));
        Ok(out)
    }
}

/// The change store: all changes since the base, saved between updates.
pub fn load_changes(path: &Path) -> Result<ChangeSet> {
    let mut bytes = Vec::new();
    match std::fs::File::open(path) {
        Ok(mut f) => {
            f.read_to_end(&mut bytes)?;
            postcard::from_bytes(&bytes)
                .with_context(|| format!("corrupt change store '{}'", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ChangeSet::default()),
        Err(e) => Err(e.into()),
    }
}

/// Save the change store atomically.
pub fn save_changes(path: &Path, changes: &ChangeSet) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&postcard::to_allocvec(changes)?)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pbf_write::{blob, header_block, primitive_block};

    fn node(id: i64) -> Elem {
        Elem::Node {
            id,
            lon: id as i32 * 10,
            lat: id as i32 * 20,
            tags: vec![],
        }
    }

    fn way(id: i64, refs: &[i64]) -> Elem {
        Elem::Way {
            id,
            refs: refs.to_vec(),
            tags: vec![("highway".into(), "residential".into())],
        }
    }

    /// A PBF with nodes 1..=300 in blocks of 100, and ways 1..=20 in one.
    fn base(dir: &Path) -> PathBuf {
        let path = dir.join("base.osm.pbf");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&blob("OSMHeader", &header_block(None)).unwrap())
            .unwrap();
        for start in [1, 101, 201] {
            let nodes: Vec<Elem> = (start..start + 100).map(node).collect();
            f.write_all(&blob("OSMData", &primitive_block(&nodes)).unwrap())
                .unwrap();
        }
        let ways: Vec<Elem> = (1..=20).map(|i| way(i, &[i, i + 1])).collect();
        f.write_all(&blob("OSMData", &primitive_block(&ways)).unwrap())
            .unwrap();
        path
    }

    #[test]
    fn reads_base_and_changes() {
        let dir = tempfile::tempdir().unwrap();
        let pbf = base(dir.path());
        let saved = dir.path().join("blocks");
        let index = BlockIndex::load_or_build(&pbf, &saved).unwrap();
        assert_eq!(index.blocks.len(), 4);
        // Saved and reused while the file is unchanged.
        assert!(saved.exists());
        assert_eq!(BlockIndex::load_or_build(&pbf, &saved).unwrap(), index);

        let mut changes = ChangeSet::default();
        changes.upsert(Elem::Node {
            id: 150,
            lon: 1,
            lat: 2,
            tags: vec![],
        });
        changes.delete(Kind::Node, 151);
        changes.upsert(node(5000)); // created
        changes.upsert(way(7, &[150, 5000]));
        let source = ElementSource::new(&pbf, &index, &changes);

        let nodes = source
            .get(Kind::Node, &[1, 100, 101, 150, 151, 300, 301, 5000])
            .unwrap();
        assert_eq!(
            nodes.keys().copied().collect::<Vec<_>>(),
            vec![1, 100, 101, 150, 300, 5000]
        );
        assert_eq!(nodes[&100], node(100));
        assert!(matches!(nodes[&150], Elem::Node { lon: 1, lat: 2, .. }));
        let ways = source.get(Kind::Way, &[1, 7, 8, 20, 99]).unwrap();
        assert_eq!(ways[&7], way(7, &[150, 5000]));
        assert_eq!(ways[&8], way(8, &[8, 9]));
        // The first and last element of a kind (regression: the first way
        // block follows a node block).
        assert_eq!(ways.get(&1), Some(&way(1, &[1, 2])));
        assert_eq!(ways.get(&20), Some(&way(20, &[20, 21])));
        assert_eq!(ways.len(), 4);
        let edges = source.get(Kind::Node, &[200, 201]).unwrap();
        assert_eq!(edges.len(), 2, "last and first id of adjacent blocks");
    }

    #[test]
    fn change_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("changes.bin");
        assert!(load_changes(&path).unwrap().is_empty());
        let mut a = ChangeSet::default();
        a.upsert(node(1));
        a.delete(Kind::Way, 2);
        let mut b = ChangeSet::default();
        b.upsert(way(2, &[1]));
        a.merge(b);
        save_changes(&path, &a).unwrap();
        let back = load_changes(&path).unwrap();
        assert_eq!(back, a);
        assert_eq!(back.ways[&2], Some(way(2, &[1])), "later diff wins");
    }
}
