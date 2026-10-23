//! Registered sources, downloads and diff updates (feature `update`).
//!
//! This is what `geors import` / `geors update` do: a source (a file, a URL
//! or `geofabrik:<region>`) is registered in `<data>/sources.json`;
//! [`update`] checks every source and applies replication diffs (or
//! downloads in full) when a new version is published.
//!
//! ```no_run
//! use geors::update::{AddOptions, UpdateOptions, add_source, update};
//!
//! add_source("data", "geofabrik:europe/liechtenstein", &AddOptions::default())?;
//! for report in update("data", &UpdateOptions::default())? {
//!     println!("{}: {:?}", report.name, report.outcome);
//! }
//! # Ok::<(), geors::Error>(())
//! ```

use std::path::Path;

pub use geors_update::{
    AddOptions, Outcome, Registry, Source, SourceReport, SourceState, UpdateOptions, Version,
};

use crate::{Error, Result};

fn err(e: anyhow::Error) -> Error {
    Error::Import(format!("{e:#}"))
}

/// Import `input` (a local file, an http(s) URL or `geofabrik:<region>`)
/// and, unless `options.track` is off, register it for [`update`].
/// Returns the partitions written.
pub fn add_source(
    data_dir: impl AsRef<Path>,
    input: &str,
    options: &AddOptions,
) -> Result<Vec<String>> {
    geors_update::import_location(data_dir.as_ref(), input, options).map_err(err)
}

/// Check every registered source (or `options.sources`) and re-import the
/// ones that changed, by diffs where possible. Running geocoders pick the
/// result up with [`Geocoder::reload_if_changed`](crate::Geocoder::reload_if_changed).
pub fn update(data_dir: impl AsRef<Path>, options: &UpdateOptions) -> Result<Vec<SourceReport>> {
    geors_update::update(data_dir.as_ref(), options).map_err(err)
}

/// Stop tracking a source (its partitions are kept). Returns whether it
/// was registered.
pub fn remove_source(data_dir: impl AsRef<Path>, name: &str) -> Result<bool> {
    geors_update::remove_source(data_dir.as_ref(), name).map_err(err)
}

/// The registered sources.
pub fn sources(data_dir: impl AsRef<Path>) -> Result<Registry> {
    Registry::load(data_dir.as_ref()).map_err(err)
}
