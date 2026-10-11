//! Check reading elements from "base + changes" against a merged file.
//!
//!   cargo run --release -p geors-update --example verify_elements -- BASE.osm.pbf MERGED.osm.pbf
//!
//! MERGED is BASE with the published diffs up to its sequence number merged
//! in (for example from `diff_stats` with `GEORS_APPLY_TO`). Reads a sample
//! through [`ElementSource`] (all nodes of changed ways, all changed
//! elements, every 500th element of MERGED) and compares each with MERGED.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use geors_update::elements::{BlockIndex, ElementSource};
use geors_update::osm::{Elem, Kind};
use geors_update::{remote::Http, replicate};

fn sorted_tags(mut e: Elem) -> Elem {
    match &mut e {
        Elem::Node { tags, .. } | Elem::Way { tags, .. } | Elem::Relation { tags, .. } => {
            tags.sort()
        }
    }
    e
}

fn main() -> Result<()> {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    let [base, merged] = args.as_slice() else {
        anyhow::bail!("usage: verify_elements BASE.osm.pbf MERGED.osm.pbf");
    };
    let from = replicate::read_replication(base)?.context("base: no replication header")?;
    let to = replicate::read_replication(merged)?.context("merged: no replication header")?;
    let changes =
        replicate::fetch_changes(&Http::new(), &from.base_url, from.sequence, to.sequence)?;

    let t = Instant::now();
    let index = BlockIndex::build(base)?;
    println!("block index built in {:?}", t.elapsed());

    // The sample: changed elements, nodes of changed ways, every 500th.
    let mut sample: BTreeMap<Kind, BTreeSet<i64>> = BTreeMap::new();
    for (kind, map) in [
        (Kind::Node, &changes.nodes),
        (Kind::Way, &changes.ways),
        (Kind::Relation, &changes.relations),
    ] {
        sample.entry(kind).or_default().extend(map.keys());
    }
    for e in changes.ways.values().flatten() {
        if let Elem::Way { refs, .. } = e {
            sample.entry(Kind::Node).or_default().extend(refs);
        }
    }
    let mut expected: BTreeMap<(Kind, i64), Elem> = BTreeMap::new();
    let mut n = 0u64;
    osmpbf::ElementReader::from_path(merged)?.for_each(|el| {
        n += 1;
        let Some(e) = Elem::from_pbf(&el) else { return };
        let key = (e.kind(), e.id());
        if n.is_multiple_of(500) {
            sample.entry(key.0).or_default().insert(key.1);
        }
        if sample.get(&key.0).is_some_and(|s| s.contains(&key.1)) {
            expected.insert(key, sorted_tags(e));
        }
    })?;

    let source = ElementSource::new(base, &index, &changes);
    let (mut checked, mut wrong) = (0usize, 0usize);
    let t = Instant::now();
    for (kind, ids) in &sample {
        let ids: Vec<i64> = ids.iter().copied().collect();
        let got = source.get(*kind, &ids)?;
        for id in ids {
            checked += 1;
            let want = expected.get(&(*kind, id));
            let have = got.get(&id).cloned().map(sorted_tags);
            if want != have.as_ref() {
                wrong += 1;
                if wrong <= 5 {
                    println!("MISMATCH {kind:?} {id}: want {want:?}, have {have:?}");
                }
            }
        }
    }
    println!(
        "{checked} elements read through base + changes in {:?}: {wrong} differ from the merged file",
        t.elapsed()
    );
    Ok(())
}
