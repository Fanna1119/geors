//! Diff (replication) updates: download the OsmChange files published since
//! the local copy was made and merge them into it, instead of downloading
//! the whole extract again.
//!
//! Geofabrik publishes one `.osc.gz` per extract update (daily) under
//! `<region>-updates/AAA/BBB/CCC.osc.gz`, with `state.txt` naming the latest
//! sequence number, and keeps them for 100 days. The extract's PBF header
//! records the replication URL and sequence it corresponds to.

use std::collections::btree_map;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::iter::Peekable;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use osmpbf::{BlobDecode, BlobReader};
use rayon::prelude::*;
use tracing::info;

use crate::osm::{ChangeSet, Elem, Kind};
use crate::pbf_write::{self, BLOCK_SIZE, Replication};
use crate::{osc, remote};

/// Blobs decoded / blocks encoded per parallel batch.
const BATCH: usize = 64;

/// Replication state from a PBF header, if the file has one.
pub fn read_replication(path: &Path) -> Result<Option<Replication>> {
    let mut reader =
        BlobReader::from_path(path).with_context(|| format!("cannot open '{}'", path.display()))?;
    let Some(first) = reader.next() else {
        return Ok(None);
    };
    let header = first?.to_headerblock()?;
    Ok(
        match (
            header.osmosis_replication_base_url(),
            header.osmosis_replication_sequence_number(),
        ) {
            (Some(url), Some(seq)) if seq >= 0 && !url.is_empty() => Some(Replication {
                base_url: url.trim_end_matches('/').to_string(),
                sequence: seq as u64,
                timestamp: header.osmosis_replication_timestamp().unwrap_or(0),
            }),
            _ => None,
        },
    )
}

/// `000/004/932` for sequence 4932.
pub fn sequence_path(seq: u64) -> String {
    let s = format!("{seq:09}");
    format!("{}/{}/{}", &s[0..3], &s[3..6], &s[6..9])
}

/// Latest published state: `(sequence, unix timestamp)`.
pub fn latest_state(http: &remote::Http, base_url: &str) -> Result<(u64, i64)> {
    let text = http
        .get_text_required(&format!("{base_url}/state.txt"))
        .context("cannot read replication state")?;
    let (seq, ts) = remote::parse_state(&text);
    let seq = seq.context("state.txt without sequenceNumber")?;
    Ok((seq, ts.as_deref().map(parse_timestamp).unwrap_or(0)))
}

/// `2026-10-08T20:21:06Z` -> unix seconds (0 if unparsable).
fn parse_timestamp(s: &str) -> i64 {
    let n = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    let (Some(y), Some(m), Some(d), Some(hh), Some(mm), Some(ss)) =
        (n(0..4), n(5..7), n(8..10), n(11..13), n(14..16), n(17..19))
    else {
        return 0;
    };
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hh * 3600 + mm * 60 + ss
}

/// Download and combine the diffs `from + 1 ..= to`.
pub fn fetch_changes(http: &remote::Http, base_url: &str, from: u64, to: u64) -> Result<ChangeSet> {
    let t = Instant::now();
    let mut changes = ChangeSet::default();
    let mut bytes = 0usize;
    for seq in from + 1..=to {
        let url = format!("{base_url}/{}.osc.gz", sequence_path(seq));
        let gz = http
            .get_bytes(&url)
            .with_context(|| format!("cannot download diff {seq}"))?;
        bytes += gz.len();
        let reader = BufReader::new(flate2::read::MultiGzDecoder::new(&gz[..]));
        osc::apply(reader, &mut changes).with_context(|| format!("invalid diff {url}"))?;
    }
    info!(
        diffs = to - from,
        download_kb = bytes / 1024,
        changed_elements = changes.len(),
        elapsed = ?t.elapsed(),
        "diffs downloaded"
    );
    Ok(changes)
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MergeStats {
    pub elements: u64,
    pub modified: u64,
    pub created: u64,
    pub deleted: u64,
}

/// Streaming merge of a sorted PBF with a change set.
struct Merger {
    pending: [Peekable<btree_map::IntoIter<i64, Option<Elem>>>; 3],
    /// Kinds whose remaining changes have been flushed.
    kind: usize,
    last: Option<(Kind, i64)>,
    block: Vec<Elem>,
    ready: Vec<Vec<Elem>>,
    stats: MergeStats,
}

impl Merger {
    fn emit(&mut self, e: Elem) {
        if let Some(first) = self.block.first()
            && (first.kind() != e.kind() || self.block.len() >= BLOCK_SIZE)
        {
            self.ready.push(std::mem::take(&mut self.block));
        }
        self.stats.elements += 1;
        self.block.push(e);
    }

    /// Emit pending changes of kind `k` with id below `limit` (creations).
    fn emit_created(&mut self, k: usize, limit: Option<i64>) {
        while let Some((id, _)) = self.pending[k].peek() {
            if limit.is_some_and(|l| *id >= l) {
                break;
            }
            let (_, change) = self.pending[k].next().unwrap();
            if let Some(e) = change {
                self.stats.created += 1;
                self.emit(e);
            }
        }
    }

    fn push(&mut self, e: Elem) -> Result<()> {
        let k = e.kind() as usize;
        // Flush whole kinds that come before this one.
        while self.kind < k {
            self.emit_created(self.kind, None);
            self.kind += 1;
        }
        if let Some((lk, lid)) = self.last
            && (e.kind() < lk || (e.kind() == lk && e.id() <= lid))
        {
            bail!("base file is not sorted by type and id; diffs cannot be applied");
        }
        self.last = Some((e.kind(), e.id()));
        self.emit_created(k, Some(e.id()));
        if self.pending[k].peek().is_some_and(|(id, _)| *id == e.id()) {
            match self.pending[k].next().unwrap().1 {
                Some(new) => {
                    self.stats.modified += 1;
                    self.emit(new);
                }
                None => self.stats.deleted += 1,
            }
        } else {
            self.emit(e);
        }
        Ok(())
    }

    fn finish(&mut self) {
        while self.kind < 3 {
            self.emit_created(self.kind, None);
            self.kind += 1;
        }
        if !self.block.is_empty() {
            self.ready.push(std::mem::take(&mut self.block));
        }
    }
}

fn write_blocks(out: &mut impl Write, blocks: Vec<Vec<Elem>>) -> Result<()> {
    let encoded: Vec<std::io::Result<Vec<u8>>> = blocks
        .par_iter()
        .map(|b| pbf_write::blob("OSMData", &pbf_write::primitive_block(b)))
        .collect();
    for blob in encoded {
        out.write_all(&blob?)?;
    }
    Ok(())
}

/// Write `base` with `changes` applied to `out`, stamped with `replication`.
/// Decoding and compression run on all cores; memory stays at a few
/// batches of blocks.
pub fn apply_changes(
    base: &Path,
    changes: ChangeSet,
    out: &Path,
    replication: &Replication,
) -> Result<MergeStats> {
    let t = Instant::now();
    let mut reader =
        BlobReader::from_path(base).with_context(|| format!("cannot open '{}'", base.display()))?;
    let mut file = BufWriter::with_capacity(1 << 20, File::create(out)?);
    file.write_all(&pbf_write::blob(
        "OSMHeader",
        &pbf_write::header_block(Some(replication)),
    )?)?;
    let ChangeSet {
        nodes,
        ways,
        relations,
    } = changes;
    let mut m = Merger {
        pending: [
            nodes.into_iter().peekable(),
            ways.into_iter().peekable(),
            relations.into_iter().peekable(),
        ],
        kind: 0,
        last: None,
        block: Vec::new(),
        ready: Vec::new(),
        stats: MergeStats::default(),
    };
    loop {
        let blobs: Vec<osmpbf::Blob> = reader
            .by_ref()
            .take(BATCH)
            .collect::<std::result::Result<_, _>>()?;
        if blobs.is_empty() {
            break;
        }
        let decoded: Vec<Vec<Elem>> = blobs
            .par_iter()
            .map(|b| -> Result<Vec<Elem>> {
                Ok(match b.decode()? {
                    BlobDecode::OsmData(block) => block
                        .elements()
                        .filter_map(|e| Elem::from_pbf(&e))
                        .collect(),
                    _ => Vec::new(),
                })
            })
            .collect::<Result<_>>()?;
        for e in decoded.into_iter().flatten() {
            m.push(e)?;
        }
        write_blocks(&mut file, std::mem::take(&mut m.ready))?;
    }
    m.finish();
    write_blocks(&mut file, std::mem::take(&mut m.ready))?;
    file.flush()?;
    info!(
        elements = m.stats.elements,
        modified = m.stats.modified,
        created = m.stats.created,
        deleted = m.stats.deleted,
        elapsed = ?t.elapsed(),
        "diffs applied"
    );
    Ok(m.stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64, lon: i32) -> Elem {
        Elem::Node {
            id,
            lon,
            lat: 0,
            tags: vec![],
        }
    }

    fn way(id: i64) -> Elem {
        Elem::Way {
            id,
            refs: vec![1, 2],
            tags: vec![],
        }
    }

    fn write(path: &Path, groups: &[Vec<Elem>]) {
        let mut f = pbf_write::blob("OSMHeader", &pbf_write::header_block(None)).unwrap();
        for g in groups {
            f.extend(pbf_write::blob("OSMData", &pbf_write::primitive_block(g)).unwrap());
        }
        std::fs::write(path, f).unwrap();
    }

    fn read(path: &Path) -> Vec<Elem> {
        let mut v = Vec::new();
        osmpbf::ElementReader::from_path(path)
            .unwrap()
            .for_each(|e| v.push(Elem::from_pbf(&e).unwrap()))
            .unwrap();
        v
    }

    #[test]
    fn merges_creates_modifies_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let (base, out) = (dir.path().join("base.pbf"), dir.path().join("out.pbf"));
        write(
            &base,
            &[
                vec![node(2, 0), node(4, 0), node(6, 0)],
                vec![way(10), way(20)],
            ],
        );
        let mut cs = ChangeSet::default();
        cs.upsert(node(1, 1)); // create before first
        cs.upsert(node(4, 4)); // modify
        cs.delete(Kind::Node, 6); // delete last
        cs.upsert(node(7, 7)); // create after last node
        cs.delete(Kind::Way, 99); // deleting something absent: no-op
        cs.upsert(way(15)); // create between
        cs.upsert(Elem::Relation {
            id: 5,
            members: vec![],
            tags: vec![],
        }); // new kind
        let rep = Replication {
            base_url: "u".into(),
            sequence: 9,
            timestamp: 1,
        };
        let stats = apply_changes(&base, cs, &out, &rep).unwrap();
        assert_eq!(
            read(&out),
            vec![
                node(1, 1),
                node(2, 0),
                node(4, 4),
                node(7, 7),
                way(10),
                way(15),
                way(20),
                Elem::Relation {
                    id: 5,
                    members: vec![],
                    tags: vec![]
                }
            ]
        );
        assert_eq!((stats.modified, stats.created, stats.deleted), (1, 4, 1));
        assert_eq!(read_replication(&out).unwrap(), Some(rep));
    }

    #[test]
    fn rejects_unsorted_base() {
        let dir = tempfile::tempdir().unwrap();
        let (base, out) = (dir.path().join("base.pbf"), dir.path().join("out.pbf"));
        write(&base, &[vec![node(5, 0)], vec![node(3, 0)]]);
        let rep = Replication::default();
        assert!(apply_changes(&base, ChangeSet::default(), &out, &rep).is_err());
    }

    #[test]
    fn paths_and_timestamps() {
        assert_eq!(sequence_path(4932), "000/004/932");
        assert_eq!(sequence_path(1_234_567_890 % 1_000_000_000), "234/567/890");
        assert_eq!(parse_timestamp("2026-10-08T20:21:06Z"), 1_791_490_866);
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), 0);
        assert_eq!(parse_timestamp("garbage"), 0);
    }
}
