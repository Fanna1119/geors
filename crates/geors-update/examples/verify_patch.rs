//! Patch = rebuild? Compare an incrementally patched import with a full
//! import of the same data.
//!
//!   cargo run --release -p geors-update --example verify_patch -- BASE.osm.pbf MERGED.osm.pbf WORK_DIR
//!
//! MERGED is BASE with the published diffs up to its sequence merged in
//! (`diff_stats` with `GEORS_APPLY_TO` makes one). Imports BASE keeping the
//! hierarchy, patches it with the diffs, imports MERGED in full, and
//! compares the places as served (main minus tombstones, plus patch).
//! The comparison streams through files sorted on disk (`sort`, `comm`), so
//! it works for Germany-sized data.
//!
//! `REUSE=1` keeps the two imports in WORK_DIR from an earlier run and only
//! rebuilds the patch and compares.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use geors_ingest::hierarchy::Hierarchy;
use geors_ingest::{ImportOptions, import_pbf};
use geors_update::patch::build_patch;
use geors_update::{remote::Http, replicate};

fn import(pbf: &Path, data: &Path, state: Option<&Path>) -> Result<()> {
    let opts = ImportOptions {
        state_dir: state.map(Path::to_path_buf),
        ..Default::default()
    };
    let import = import_pbf(pbf, &opts)?;
    for c in &import.countries {
        geors_index::write_partition(
            data,
            geors_index::PartitionInput {
                country_code: c.country_code.clone(),
                country_name: c.country_name.clone(),
                source: "verify".into(),
                places: c.places.clone(),
                admins: c.admins.clone(),
            },
        )?;
    }
    Ok(())
}

/// The places of a country as served, one normalised JSON line each,
/// written to `out` sorted.
fn served(data: &Path, cc: &str, out: &Path) -> Result<u64> {
    let (main, patch) = geors_index::Partition::open_with_patch(&data.join(cc))?;
    let unsorted = out.with_extension("unsorted");
    let mut w = std::io::BufWriter::new(std::fs::File::create(&unsorted)?);
    let mut n = 0;
    for p in std::iter::once(&main).chain(patch.as_ref()) {
        for id in (0..p.len()).filter(|&id| !p.is_deleted(id)) {
            let rec = p.record(id).context("record")?;
            let place = p.doc(&rec, false)?;
            let mut v = serde_json::to_value(&place)?;
            let mut parents: Vec<String> = place
                .parents
                .iter()
                .filter_map(|&u| p.admins.get(u as usize))
                .map(|a| format!("{}:{}", a.layer, a.name))
                .collect();
            parents.sort();
            v["parents"] = serde_json::json!(parents);
            serde_json::to_writer(&mut w, &v)?;
            w.write_all(b"\n")?;
            n += 1;
        }
    }
    w.flush()?;
    drop(w);
    let ok = Command::new("sort")
        .env("LC_ALL", "C")
        .arg("-o")
        .arg(out)
        .arg(&unsorted)
        .status()?
        .success();
    anyhow::ensure!(ok, "sort failed");
    std::fs::remove_file(&unsorted)?;
    Ok(n)
}

/// Lines only in `a` (`comm -23`).
fn only_in(a: &Path, b: &Path) -> Result<Vec<String>> {
    let out = Command::new("comm")
        .env("LC_ALL", "C")
        .arg("-23")
        .arg(a)
        .arg(b)
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

fn main() -> Result<()> {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    let [base, merged, work] = args.as_slice() else {
        anyhow::bail!("usage: verify_patch BASE.osm.pbf MERGED.osm.pbf WORK_DIR");
    };
    let (patched, full, state) = (work.join("patched"), work.join("full"), work.join("state"));
    let reuse = std::env::var("REUSE").is_ok();
    if !reuse {
        let _ = std::fs::remove_dir_all(work);
        std::fs::create_dir_all(&patched)?;
        std::fs::create_dir_all(&full)?;
        let t = Instant::now();
        import(base, &patched, Some(&state))?;
        println!("base imported (hierarchy kept) in {:?}", t.elapsed());
    }

    let from = replicate::read_replication(base)?.context("base: no replication header")?;
    let to = replicate::read_replication(merged)?.context("merged: no replication header")?;
    let changes =
        replicate::fetch_changes(&Http::new(), &from.base_url, from.sequence, to.sequence)?;
    let t = Instant::now();
    let hierarchy = Arc::new(Hierarchy::load(&state)?);
    let stats = build_patch(
        base,
        &changes,
        hierarchy,
        &patched,
        &work.join("patch-work"),
    )?;
    println!("PATCH in {:?}: {stats:?}", t.elapsed());

    if !reuse {
        let t = Instant::now();
        import(merged, &full, None)?;
        println!("merged imported in full in {:?}", t.elapsed());
    }

    for cc in geors_index::engine::list_partitions(&full)? {
        let (a, b) = (
            work.join(format!("{cc}-patched.jsonl")),
            work.join(format!("{cc}-full.jsonl")),
        );
        served(&patched, &cc, &a)?;
        let n = served(&full, &cc, &b)?;
        let only_patch = only_in(&a, &b)?;
        let only_full = only_in(&b, &a)?;
        println!(
            "{cc}: {n} places served; {} only in the patched data, {} only in the full rebuild",
            only_patch.len(),
            only_full.len()
        );
        for (label, list) in [("patched", &only_patch), ("full", &only_full)] {
            for line in list.iter().take(3) {
                println!("  {label}: {}", &line[..line.len().min(400)]);
            }
        }
    }
    Ok(())
}
