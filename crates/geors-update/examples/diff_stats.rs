//! What do replication diffs change, and what would patching them take?
//!
//!   cargo run --release -p geors-update --example diff_stats -- BASE.osm.pbf [MAX_DIFFS]
//!
//! With `GEORS_APPLY_TO=out.osm.pbf`, also times merging the diffs into the
//! base (what `geors update` does today before re-importing).
//!
//! Downloads the published diffs after the base's sequence number, finds
//! the old version of every changed element in one pass over the base,
//! and reports: created / modified / deleted per kind; for modified nodes
//! whether they moved or only changed tags; and the hard case for
//! incremental updates: ways in the base that use a moved node but are not
//! in the diff themselves (their shape changes silently).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use geors_update::osm::Elem;
use geors_update::{remote::Http, replicate};

fn tagged(tags: &[(String, String)]) -> bool {
    tags.iter()
        .any(|(k, _)| !matches!(k.as_str(), "source" | "created_by" | "fixme" | "note"))
}

/// Tags that make a way a searchable place (roughly the importer's rules).
fn searchable(tags: &[(String, String)]) -> bool {
    tags.iter().any(|(k, _)| {
        matches!(
            k.as_str(),
            "name" | "addr:housenumber" | "addr:interpolation" | "place" | "boundary"
        )
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let base = PathBuf::from(
        args.get(1)
            .context("usage: diff_stats BASE.osm.pbf [MAX_DIFFS]")?,
    );
    let max: Option<u64> = args.get(2).map(|s| s.parse()).transpose()?;
    let rep = replicate::read_replication(&base)?.context("base has no replication header")?;
    let http = Http::new();
    let (latest, _) = replicate::latest_state(&http, &rep.base_url)?;
    let to = max.map_or(latest, |m| (rep.sequence + m).min(latest));
    println!(
        "base sequence {}, applying {}..={} ({} diffs)",
        rep.sequence,
        rep.sequence + 1,
        to,
        to - rep.sequence
    );
    let t = std::time::Instant::now();
    let changes = replicate::fetch_changes(&http, &rep.base_url, rep.sequence, to)?;
    println!("downloaded and parsed in {:?}", t.elapsed());

    // One pass over the base: old versions of changed elements, and the
    // ways that use a changed node.
    let changed_nodes: HashSet<i64> = changes.nodes.keys().copied().collect();
    let mut old_nodes: HashMap<i64, (i32, i32, bool)> = HashMap::new();
    let mut old_ways: HashSet<i64> = HashSet::new();
    let mut old_rels: HashSet<i64> = HashSet::new();
    // Admin / postcode boundaries in the base: relation -> (admin_level, member ways).
    let mut boundaries: HashMap<i64, (String, Vec<i64>)> = HashMap::new();
    // way -> (searchable, uses a changed node)
    let mut ways_on_changed: HashMap<i64, bool> = HashMap::new();
    let t = std::time::Instant::now();
    osmpbf::ElementReader::from_path(&base)?.for_each(|el| match Elem::from_pbf(&el) {
        Some(Elem::Node { id, lon, lat, tags }) if changed_nodes.contains(&id) => {
            old_nodes.insert(id, (lon, lat, tagged(&tags)));
        }
        Some(Elem::Way { id, refs, tags }) => {
            if changes.ways.contains_key(&id) {
                old_ways.insert(id);
            }
            if refs.iter().any(|r| changed_nodes.contains(r)) {
                ways_on_changed.insert(id, searchable(&tags));
            }
        }
        Some(Elem::Relation { id, members, tags }) => {
            if changes.relations.contains_key(&id) {
                old_rels.insert(id);
            }
            let tag = |k: &str| tags.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
            if matches!(
                tag("boundary").as_deref(),
                Some("administrative" | "postal_code")
            ) {
                let level = tag("admin_level").unwrap_or_else(|| "postcode".into());
                let ways = members
                    .iter()
                    .filter(|m| m.kind == geors_update::osm::Kind::Way)
                    .map(|m| m.id)
                    .collect();
                boundaries.insert(id, (level, ways));
            }
        }
        _ => {}
    })?;
    println!("scanned base in {:?}", t.elapsed());

    let count = |map: &std::collections::BTreeMap<i64, Option<Elem>>, old: &dyn Fn(i64) -> bool| {
        let (mut created, mut modified, mut deleted) = (0, 0, 0);
        for (id, e) in map {
            match (e, old(*id)) {
                (None, _) => deleted += 1,
                (Some(_), true) => modified += 1,
                (Some(_), false) => created += 1,
            }
        }
        (created, modified, deleted)
    };
    println!("\n             created  modified  deleted");
    for (name, (c, m, d)) in [
        (
            "nodes",
            count(&changes.nodes, &|id| old_nodes.contains_key(&id)),
        ),
        ("ways", count(&changes.ways, &|id| old_ways.contains(&id))),
        (
            "relations",
            count(&changes.relations, &|id| old_rels.contains(&id)),
        ),
    ] {
        println!("{name:<12} {c:>8} {m:>9} {d:>8}");
    }

    // Modified nodes: moved, and/or tags.
    let (mut moved, mut retagged, mut moved_untagged) = (0, 0, HashSet::new());
    for (id, e) in &changes.nodes {
        if let (Some(Elem::Node { lon, lat, tags, .. }), Some(&(olon, olat, otagged))) =
            (e, old_nodes.get(id))
        {
            let did_move = (*lon, *lat) != (olon, olat);
            if did_move {
                moved += 1;
                if !tagged(tags) && !otagged {
                    moved_untagged.insert(*id);
                }
            }
            if tagged(tags) || otagged {
                retagged += 1;
            }
        }
    }
    println!(
        "\nmodified nodes: {moved} moved ({} untagged geometry nodes), {retagged} tagged",
        moved_untagged.len()
    );

    // Ways reshaped without being in the diff: they use a changed node.
    let silent: Vec<(&i64, &bool)> = ways_on_changed
        .iter()
        .filter(|(w, _)| !changes.ways.contains_key(w))
        .collect();
    let silent_searchable = silent.iter().filter(|(_, s)| **s).count();
    println!(
        "ways using a changed node: {} ({} not in the diff, of which {} searchable)",
        ways_on_changed.len(),
        silent.len(),
        silent_searchable
    );
    let searchable_in_diff = changes
        .ways
        .values()
        .flatten()
        .filter(|e| matches!(e, Elem::Way { tags, .. } if searchable(tags)))
        .count();
    let searchable_nodes = changes
        .nodes
        .values()
        .flatten()
        .filter(|e| matches!(e, Elem::Node { tags, .. } if searchable(tags)))
        .count();
    println!(
        "searchable elements in the diff: {searchable_nodes} nodes, {searchable_in_diff} ways, {} relations",
        changes.relations.len()
    );

    // Boundaries whose shape may have changed: the relation itself, a
    // member way, or a node of a member way changed.
    let reshaped_ways: HashSet<i64> = ways_on_changed
        .keys()
        .copied()
        .chain(changes.ways.keys().copied())
        .collect();
    let mut by_level: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    for (id, (level, ways)) in &boundaries {
        let rel = changes.relations.contains_key(id);
        let shape = ways.iter().any(|w| reshaped_ways.contains(w));
        if rel || shape {
            let e = by_level.entry(level.clone()).or_default();
            e.0 += 1;
            e.1 += shape as usize;
        }
    }
    println!("\nboundaries touched (admin_level: touched, of which shape changed):");
    for (level, (touched, shape)) in &by_level {
        println!("  {level:>9}: {touched:>5} {shape:>5}");
    }
    if let Ok(out) = std::env::var("GEORS_APPLY_TO") {
        let t = std::time::Instant::now();
        let new_rep = geors_update::pbf_write::Replication {
            base_url: rep.base_url.clone(),
            sequence: to,
            timestamp: 0,
        };
        let stats = replicate::apply_changes(&base, changes, std::path::Path::new(&out), &new_rep)?;
        println!(
            "\nmerged into a new base in {:?} ({} elements)",
            t.elapsed(),
            stats.elements
        );
    }
    Ok(())
}
