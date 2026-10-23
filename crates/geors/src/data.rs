//! Inspecting and maintaining a data directory without opening a
//! [`Geocoder`](crate::Geocoder): listing partitions, exporting places,
//! adding files newer versions write at import.

use std::path::{Path, PathBuf};

use geors_index::Partition;

use crate::{Error, PartitionInfo, Place, Result};

impl PartitionInfo {
    pub(crate) fn of(p: &Partition) -> Self {
        let m = &p.meta;
        PartitionInfo {
            name: p.name(),
            country_code: m.country_code.clone(),
            country_name: m.country_name.clone(),
            places: m.num_places,
            layers: m.layers.clone(),
            bbox: m.bbox,
            source: m.source.clone(),
            created_unix: m.created_unix,
        }
    }
}

/// A partition directory found in a data directory.
#[derive(Debug)]
pub struct PartitionEntry {
    /// Directory name: the country code, or `<cc>@<source>`.
    pub name: String,
    pub dir: PathBuf,
    /// The partition, or why it cannot be opened (written by another
    /// format version, damaged, ...).
    pub info: Result<PartitionInfo>,
}

/// Every partition directory in `data_dir`, sorted by name, including
/// ones that cannot be opened.
pub fn partitions(data_dir: impl AsRef<Path>) -> Result<Vec<PartitionEntry>> {
    let data_dir = data_dir.as_ref();
    Ok(geors_index::engine::list_partitions(data_dir)?
        .into_iter()
        .map(|name| {
            let dir = data_dir.join(&name);
            let info = Partition::open(&dir)
                .map(|p| PartitionInfo::of(&p))
                .map_err(Error::from);
            PartitionEntry { name, dir, info }
        })
        .collect())
}

/// Add the files newer versions write at import (`importance.bin`,
/// `centers.bin`) to partitions that lack them, without re-importing, and
/// let running geocoders reload. Returns the partitions changed and the
/// files added to each.
pub fn upgrade(data_dir: impl AsRef<Path>) -> Result<Vec<(String, Vec<&'static str>)>> {
    let data_dir = data_dir.as_ref();
    let mut changed = Vec::new();
    for name in geors_index::engine::list_partitions(data_dir)? {
        let added = geors_index::partition::add_derived_files(&data_dir.join(&name))?;
        if !added.is_empty() {
            changed.push((name, added));
        }
    }
    if !changed.is_empty() {
        geors_core::storage::bump_generation(data_dir).map_err(geors_index::IndexError::from)?;
    }
    Ok(changed)
}

/// Every `every`-th place of partition `name` (a country code, or
/// `<cc>@<source>`), with its admin parents as `"<layer>:<name>"`
/// (`"city:Vaduz"`), for inspection and diffs. Stops at the first error
/// `f` returns.
pub fn export<E: From<Error>>(
    data_dir: impl AsRef<Path>,
    name: &str,
    every: u32,
    mut f: impl FnMut(Place, Vec<String>) -> Result<(), E>,
) -> Result<(), E> {
    let p = Partition::open(&data_dir.as_ref().join(name)).map_err(Error::from)?;
    for id in (0..p.len()).step_by(every.max(1) as usize) {
        let rec = p
            .record(id)
            .ok_or_else(|| Error::Import(format!("partition '{name}': corrupt record {id}")))?;
        let place = p.doc(&rec, false).map_err(Error::from)?;
        let parents = place
            .parents
            .iter()
            .filter_map(|&u| p.admins.get(u as usize))
            .map(|a| format!("{}:{}", a.layer, a.name))
            .collect();
        f(place, parents)?;
    }
    Ok(())
}
