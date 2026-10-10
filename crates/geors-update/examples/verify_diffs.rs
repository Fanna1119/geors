//! Check diff updates against reality: apply the published diffs to an old
//! extract and compare the result, element by element, with a newer
//! official extract of the same region.
//!
//!   cargo run --release -p geors-update --example verify_diffs -- OLD.osm.pbf NEW.osm.pbf
//!
//! Both files are sorted by type and id, so the comparison streams both in
//! lockstep with constant memory.

use std::path::Path;
use std::sync::mpsc::sync_channel;

use anyhow::{Context, Result, bail};
use geors_update::osm::Elem;
use geors_update::{remote::Http, replicate};

/// Elements of a PBF in file order, read on a background thread.
fn stream(path: &Path) -> impl Iterator<Item = Elem> {
    let (tx, rx) = sync_channel::<Vec<Elem>>(64);
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let mut batch = Vec::with_capacity(8192);
        osmpbf::ElementReader::from_path(&path)
            .expect("open")
            .for_each(|e| {
                let mut e = Elem::from_pbf(&e).unwrap();
                // Tag order is not significant.
                match &mut e {
                    Elem::Node { tags, .. }
                    | Elem::Way { tags, .. }
                    | Elem::Relation { tags, .. } => tags.sort(),
                }
                batch.push(e);
                if batch.len() == 8192 {
                    let _ = tx.send(std::mem::take(&mut batch));
                }
            })
            .expect("read");
        let _ = tx.send(batch);
    });
    rx.into_iter().flatten()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [old, new] = &args[..] else {
        bail!("usage: verify_diffs OLD.osm.pbf NEW.osm.pbf")
    };
    let (old, new) = (Path::new(old), Path::new(new));
    let from = replicate::read_replication(old)?.context("old file has no replication header")?;
    let to = replicate::read_replication(new)?.context("new file has no replication header")?;
    println!(
        "old: seq {}  new: seq {}  ({} diffs)",
        from.sequence,
        to.sequence,
        to.sequence - from.sequence
    );
    let http = Http::new();
    let t = std::time::Instant::now();
    let changes = replicate::fetch_changes(&http, &from.base_url, from.sequence, to.sequence)?;
    let fetched = t.elapsed();
    let merged = old.with_extension("merged.pbf");
    let stats = replicate::apply_changes(old, changes, &merged, &to)?;
    println!(
        "download {fetched:.1?}, merge {:.1?}: {stats:?}",
        t.elapsed() - fetched
    );

    let (mut a, mut b) = (stream(&merged).peekable(), stream(new).peekable());
    let (mut same, mut missing, mut extra, mut differ) = (0u64, 0u64, 0u64, 0u64);
    loop {
        let key = |e: &Elem| (e.kind(), e.id());
        match (a.peek(), b.peek()) {
            (None, None) => break,
            (Some(_), None) => {
                extra += 1;
                a.next();
            }
            (None, Some(_)) => {
                missing += 1;
                b.next();
            }
            (Some(x), Some(y)) if key(x) < key(y) => {
                extra += 1;
                a.next();
            }
            (Some(x), Some(y)) if key(x) > key(y) => {
                missing += 1;
                b.next();
            }
            _ => {
                let (x, y) = (a.next().unwrap(), b.next().unwrap());
                if x == y {
                    same += 1;
                } else {
                    if differ < 3 {
                        println!("  differs:\n    merged:   {x:?}\n    official: {y:?}");
                    }
                    differ += 1;
                }
            }
        }
    }
    println!("identical: {same} | missing: {missing} | extra: {extra} | different: {differ}");
    std::fs::remove_file(&merged)?;
    Ok(())
}
