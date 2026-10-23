//! Importing extracts into a data directory (feature `import`).
//!
//! ```no_run
//! use geors::import::{ImportOptions, import};
//!
//! let report = import("austria-latest.osm.pbf", "data", &ImportOptions::default())?;
//! println!("wrote {:?}", report.partitions);
//! for s in &report.skipped {
//!     println!("skipped {} ({} places): {}", s.country_code, s.places, s.reason);
//! }
//! # Ok::<(), geors::Error>(())
//! ```
//!
//! A running [`Geocoder`](crate::Geocoder) on the same directory picks the
//! new partitions up with [`Geocoder::reload_if_changed`](crate::Geocoder::reload_if_changed).
//! Downloading extracts and keeping them current with replication diffs is
//! the `update` feature ([`crate::update`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use geors_core::memory_limit;
pub use geors_ingest::{NodeLookup, SkippedCountry};
pub use geors_update::ImportReport;

use crate::{Error, Result};

/// How to import.
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Name of the source, used to tell apart partitions of a country that
    /// several extracts hold (`ru` and `ru@asia`). Default: from the file
    /// name (`austria-latest.osm.pbf` -> `austria`).
    pub name: Option<String>,
    /// Only these countries (ISO 3166-1 alpha-2, any case). Default: the
    /// countries the extract covers, without border slivers.
    pub countries: Vec<String>,
    /// Keep every country found, border slivers included.
    pub all_countries: bool,
    /// Country for places outside every country boundary (city extracts).
    pub default_country: Option<String>,
    /// Nominatim's `wikimedia-importance.csv.gz`, to rank by Wikipedia
    /// importance (cached in `<data>/.importance`).
    pub importance: Option<PathBuf>,
}

/// Process-wide import resources. Call once before importing; without it
/// imports size themselves from the memory limit and use all CPUs.
#[derive(Debug, Clone, Default)]
pub struct Tuning {
    /// Threads the text indexer is sized for (the rayon pool decodes and
    /// writes; build it with the same size). Default: the CPUs available.
    pub threads: Option<usize>,
    /// Memory for building the text index, in MB (minimum 15). Default: 48
    /// per thread, at most 4 threads, at most an eighth of [`memory_limit`].
    pub index_memory_mb: Option<usize>,
    /// How way node coordinates are found: `Memory` (random lookups,
    /// fastest when they fit in RAM), `Sorted` (sort-merge join, for
    /// extracts larger than RAM) or `Auto`.
    pub node_lookup: NodeLookup,
}

/// Apply [`Tuning`] for every import that follows in this process.
pub fn configure(tuning: &Tuning) {
    let threads = tuning
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .max(1);
    geors_ingest::set_default_node_lookup(tuning.node_lookup);
    // Spill files up to a quarter of the memory are read in place while
    // writing; larger ones are regrouped for sequential reads.
    let memory = memory_limit();
    geors_index::writer::set_writer_memory(
        memory.map_or(u64::MAX, |m| m / 4),
        geors_ingest::extsort::buffer_bytes() / 2,
    );
    match (tuning.index_memory_mb, memory) {
        (Some(mb), _) => geors_index::text::set_index_memory(mb * 1024 * 1024),
        // Default: 48 MB per indexing thread, at most an eighth of memory.
        (None, Some(m)) => {
            geors_index::text::set_index_memory(((m / 8) as usize).min(threads.min(4) * (48 << 20)))
        }
        (None, None) => {}
    }
}

/// Import an `.osm.pbf` file or a Nominatim/Photon JSON dump into
/// `data_dir`, one partition per country. Replaces partitions this source
/// wrote before; holds the data directory's lock while it runs.
pub fn import(
    input: impl AsRef<Path>,
    data_dir: impl AsRef<Path>,
    options: &ImportOptions,
) -> Result<ImportReport> {
    let (input, data_dir) = (input.as_ref(), data_dir.as_ref());
    let location = input.to_string_lossy();
    if location.starts_with("http://") || location.starts_with("https://") {
        return Err(Error::InvalidRequest(format!(
            "'{location}' is a URL: import a local file, or use the update feature to download"
        )));
    }
    let import_err = |e: anyhow::Error| Error::Import(format!("{e:#}"));
    std::fs::create_dir_all(data_dir)
        .map_err(|e| Error::Import(format!("cannot create '{}': {e}", data_dir.display())))?;
    let _lock = geors_update::lock(data_dir).map_err(import_err)?;
    let format = geors_update::Format::detect(&location).map_err(import_err)?;
    let countries = crate::request::countries(&options.countries)?;
    let default_country = match &options.default_country {
        Some(c) => crate::request::countries(std::slice::from_ref(c))?.pop(),
        None => None,
    };
    let importance = match &options.importance {
        Some(file) => Some(Arc::new(
            geors_ingest::wiki::WikiImportance::load(file, &data_dir.join(".importance"))
                .map_err(import_err)?,
        )),
        None => None,
    };
    let ingest = geors_ingest::ImportOptions {
        countries,
        all_countries: options.all_countries,
        default_country,
        importance,
        ..Default::default()
    };
    let label = input
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = options
        .name
        .clone()
        .unwrap_or_else(|| geors_update::default_name(&location));
    geors_update::import_file(input, format, data_dir, &ingest, &label, &name, false)
        .map_err(import_err)
}
