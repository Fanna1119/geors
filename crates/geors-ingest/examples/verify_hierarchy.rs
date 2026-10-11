//! Check that a kept address hierarchy assigns the same parents as the
//! import did.
//!
//!   cargo run --release -p geors-ingest --example verify_hierarchy -- EXTRACT.osm.pbf WORK_DIR
//!
//! Imports the extract with the hierarchy kept in `WORK_DIR/state`, writes
//! the partitions to `WORK_DIR/data`, loads the hierarchy back from disk,
//! re-assigns every place's parents from its stored centre, and compares
//! them (by layer and name) with the stored ones.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use geors_core::Layer;
use geors_ingest::hierarchy::{HierCache, Hierarchy};
use geors_ingest::{ImportOptions, import_pbf};

fn main() -> Result<()> {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    let [pbf, work] = args.as_slice() else {
        anyhow::bail!("usage: verify_hierarchy EXTRACT.osm.pbf WORK_DIR");
    };
    let (state, data) = (work.join("state"), work.join("data"));
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(&data)?;
    let opts = ImportOptions {
        state_dir: Some(state.clone()),
        ..Default::default()
    };
    let t = Instant::now();
    let import = import_pbf(pbf, &opts)?;
    let mut codes = Vec::new();
    for c in &import.countries {
        geors_index::write_partition(
            &data,
            geors_index::PartitionInput {
                country_code: c.country_code.clone(),
                country_name: c.country_name.clone(),
                source: "verify".into(),
                places: c.places.clone(),
                admins: c.admins.clone(),
            },
        )?;
        codes.push(c.country_code.clone());
    }
    drop(import);
    println!("imported in {:?}: {}", t.elapsed(), codes.join(","));

    let t = Instant::now();
    let hierarchy = Hierarchy::load(&state)?;
    println!("hierarchy loaded in {:?}", t.elapsed());
    let state_mb = std::fs::read_dir(&state)?
        .flatten()
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum::<u64>()
        / 1_048_576;
    println!("kept state: {state_mb} MB");

    let mut cache = HierCache::default();
    // layer -> (checked, differing)
    let mut stats: BTreeMap<Layer, (u64, u64)> = BTreeMap::new();
    let mut shown = 0;
    for cc in &codes {
        let part = geors_index::Partition::open(&data.join(cc)).context("open partition")?;
        let names = |admins: &[geors_core::AdminUnit], ids: &[u32]| -> Vec<String> {
            let mut v: Vec<String> = ids
                .iter()
                .filter_map(|&u| admins.get(u as usize))
                .map(|a| format!("{}:{}", a.layer, a.name))
                .collect();
            v.sort();
            v
        };
        for id in 0..part.len() {
            let rec = part.record(id).context("record")?;
            if rec.layer >= Layer::Locality {
                continue; // areas: their own unit is excluded, unknown here
            }
            let stored = part.doc(&rec, false)?;
            let mut again = stored.clone();
            again.parents.clear();
            hierarchy.assign(&mut cache, &mut again, None);
            let (a, b) = (
                names(&part.admins, &stored.parents),
                names(&hierarchy.units, &again.parents),
            );
            let e = stats.entry(rec.layer).or_default();
            e.0 += 1;
            if a != b {
                e.1 += 1;
                if shown < 5 && (rec.layer == Layer::House || std::env::var("SHOW_ALL").is_ok()) {
                    shown += 1;
                    println!(
                        "DIFF {:?} {}: stored {a:?}, again {b:?}",
                        stored.osm_type, stored.osm_id
                    );
                }
            }
        }
    }
    for (layer, (n, d)) in &stats {
        println!("{layer:>8}: {n:>9} places, {d:>6} with different parents");
    }
    Ok(())
}
