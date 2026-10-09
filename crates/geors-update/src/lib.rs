//! Data management: importing extracts (local files or URLs), remembering
//! where they came from, and keeping them up to date.
//!
//! Every import registers a [`Source`] in `<data>/sources.json`. `update`
//! checks each source for a newer published version (Geofabrik replication
//! sequence, MD5, ETag, or file mtime), downloads and re-imports changed ones,
//! and bumps `<data>/GENERATION` so running servers hot-reload.

pub mod remote;
pub mod source;

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use geors_index::{PartitionInput, write_partition};
use geors_ingest::ImportOptions;
use tracing::{info, warn};

pub use remote::Http;
pub use source::{
    Format, Registry, Source, SourceState, Version, default_name, expand_location, is_url,
};

const DOWNLOAD_DIR: &str = ".downloads";
const LOCK_FILE: &str = ".lock";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Exclusive lock on a data directory for the duration of an import/update.
/// Released when dropped (or when the process exits).
pub struct DataLock {
    _file: File,
}

pub fn lock(data_dir: &Path) -> Result<DataLock> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("cannot create data directory '{}'", data_dir.display()))?;
    let path = data_dir.join(LOCK_FILE);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open lock file '{}'", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(DataLock { _file: file }),
        Err(TryLockError::WouldBlock) => bail!(
            "another geors import/update is running on '{}'; try again when it has finished",
            data_dir.display()
        ),
        Err(TryLockError::Error(e)) => Err(e).context("cannot lock data directory"),
    }
}

/// Import a local file and write its country partitions. Returns the
/// partition codes written. Caller must hold the [`DataLock`].
pub fn import_file(
    path: &Path,
    format: Format,
    data_dir: &Path,
    opts: &ImportOptions,
    source_label: &str,
) -> Result<Vec<String>> {
    let t = Instant::now();
    info!(input = %path.display(), ?format, "importing");
    let countries = match format {
        Format::Pbf => geors_ingest::import_pbf(path, opts)?,
        Format::Dump => geors_ingest::dump::import_dump(path, opts)?,
    };
    if countries.is_empty() {
        bail!(
            "no places imported from '{}'; if the extract has no country boundary, pass --default-country",
            path.display()
        );
    }
    let mut written = Vec::new();
    for c in countries {
        let code = c.country_code.clone();
        write_partition(
            data_dir,
            PartitionInput {
                country_code: c.country_code,
                country_name: c.country_name,
                source: source_label.to_string(),
                places: c.places,
                admins: c.admins,
            },
        )?;
        written.push(code);
    }
    geors_core::storage::bump_generation(data_dir)?;
    info!(input = %source_label, partitions = %written.join(","), elapsed = ?t.elapsed(), "import finished");
    Ok(written)
}

/// Settings for registering a new source with [`import_location`].
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    pub name: Option<String>,
    pub countries: Vec<String>,
    pub all_countries: bool,
    pub default_country: Option<String>,
    /// Remember the source so `update` keeps it current.
    pub track: bool,
    /// Keep downloaded files in `<data>/.downloads`.
    pub keep_download: bool,
}

/// `geors import`: download (for URLs), import, and register the source.
pub fn import_location(data_dir: &Path, input: &str, opts: &AddOptions) -> Result<Vec<String>> {
    let location = expand_location(input);
    let format = Format::detect(&location)?;
    let location = if is_url(&location) {
        location
    } else {
        let path = Path::new(&location);
        if !path.is_file() {
            bail!("input file '{}' does not exist", path.display());
        }
        std::fs::canonicalize(path)?.to_string_lossy().to_string()
    };
    let _lock = lock(data_dir)?;
    let mut source = Source {
        name: opts.name.clone().unwrap_or_else(|| default_name(&location)),
        location,
        format,
        countries: opts.countries.clone(),
        all_countries: opts.all_countries,
        default_country: opts.default_country.clone(),
        state: SourceState::default(),
    };
    let partitions = fetch_and_import(
        &Http::new(),
        data_dir,
        &mut source,
        None,
        opts.keep_download,
    )?;
    if opts.track {
        let mut reg = Registry::load(data_dir)?;
        info!(source = %source.name, "registered source for `geors update`");
        reg.upsert(source);
        reg.save()?;
    }
    Ok(partitions)
}

/// Fetch the source's current version (if remote) and import it.
fn fetch_and_import(
    http: &Http,
    data_dir: &Path,
    source: &mut Source,
    known: Option<Version>,
    keep_download: bool,
) -> Result<Vec<String>> {
    let opts = source.import_options();
    let (partitions, version) = if source.is_remote() {
        let dir = data_dir.join(DOWNLOAD_DIR);
        std::fs::create_dir_all(&dir)?;
        let dest: PathBuf = dir.join(format!("{}.{}", source.name, source.format.extension()));
        let mut version = match known {
            Some(v) => v,
            None => http.check(&source.location)?,
        };
        let md5 = match http.download(&source.location, &dest, version.md5.as_deref()) {
            Ok(md5) => md5,
            // The file may have been republished between check and download.
            Err(e) if e.to_string().contains("checksum mismatch") => {
                warn!(error = %e, "re-checking source and retrying once");
                version = http.check(&source.location)?;
                http.download(&source.location, &dest, version.md5.as_deref())?
            }
            Err(e) => return Err(e),
        };
        version.md5 = Some(md5);
        let label = source
            .location
            .rsplit('/')
            .next()
            .unwrap_or(&source.name)
            .to_string();
        let result = import_file(&dest, source.format, data_dir, &opts, &label);
        if !keep_download && result.is_ok() {
            let _ = std::fs::remove_file(&dest);
        }
        (result?, version)
    } else {
        let path = Path::new(&source.location);
        let version = remote::check_local(path)?;
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        (
            import_file(path, source.format, data_dir, &opts, &label)?,
            version,
        )
    };
    for old in &source.state.partitions {
        if !partitions.contains(old) {
            warn!(source = %source.name, partition = %old, "partition no longer produced by this source; left in place");
        }
    }
    source.state.version = Some(version);
    source.state.partitions = partitions.clone();
    source.state.imported_at = Some(now());
    source.state.last_error = None;
    Ok(partitions)
}

#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    /// Re-import even if the source looks unchanged.
    pub force: bool,
    /// Only these sources (by name). Empty = all.
    pub sources: Vec<String>,
    pub keep_download: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Updated { partitions: Vec<String> },
    UpToDate,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct SourceReport {
    pub name: String,
    pub outcome: Outcome,
}

impl SourceReport {
    pub fn updated(&self) -> bool {
        matches!(self.outcome, Outcome::Updated { .. })
    }
}

/// Check every registered source and re-import those that changed.
/// A failing source does not stop the others; its error is recorded.
pub fn update(data_dir: &Path, opts: &UpdateOptions) -> Result<Vec<SourceReport>> {
    let _lock = lock(data_dir)?;
    let mut reg = Registry::load(data_dir)?;
    if reg.sources.is_empty() {
        bail!(
            "no sources registered in '{}'; import one first, e.g. `geors import geofabrik:europe/liechtenstein`",
            data_dir.display()
        );
    }
    for name in &opts.sources {
        if !reg.sources.iter().any(|s| &s.name == name) {
            let known: Vec<&str> = reg.sources.iter().map(|s| s.name.as_str()).collect();
            bail!("unknown source '{name}' (known: {})", known.join(", "));
        }
    }
    let http = Http::new();
    let names: Vec<String> = reg
        .sources
        .iter()
        .map(|s| s.name.clone())
        .filter(|n| opts.sources.is_empty() || opts.sources.contains(n))
        .collect();
    let mut reports = Vec::new();
    for name in names {
        let source = reg.get_mut(&name).unwrap();
        source.state.checked_at = Some(now());
        let outcome = match check(&http, source) {
            Err(e) => Outcome::Failed(format!("{e:#}")),
            Ok(current) => {
                let unchanged = source
                    .state
                    .version
                    .as_ref()
                    .is_some_and(|v| !current.differs_from(v));
                if unchanged && !opts.force {
                    info!(source = %name, timestamp = current.timestamp.as_deref().unwrap_or("-"), "up to date");
                    Outcome::UpToDate
                } else {
                    info!(source = %name, sequence = ?current.sequence, timestamp = current.timestamp.as_deref().unwrap_or("-"), "new version available");
                    match fetch_and_import(
                        &http,
                        data_dir,
                        source,
                        Some(current),
                        opts.keep_download,
                    ) {
                        Ok(partitions) => Outcome::Updated { partitions },
                        Err(e) => Outcome::Failed(format!("{e:#}")),
                    }
                }
            }
        };
        if let Outcome::Failed(e) = &outcome {
            warn!(source = %name, error = %e, "update failed");
            source.state.last_error = Some(e.clone());
        }
        reg.save()?;
        reports.push(SourceReport { name, outcome });
    }
    Ok(reports)
}

fn check(http: &Http, source: &Source) -> Result<Version> {
    if source.is_remote() {
        http.check(&source.location)
    } else {
        remote::check_local(Path::new(&source.location))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"{"type":"NominatimDumpFile","content":{"version":"0.1.0"}}
{"type":"Place","content":[{"place_id":1,"object_type":"N","object_id":10,"osm_key":"place","osm_value":"town","rank_address":16,"name":{"name":"Vaduz"},"country_code":"li","centroid":[9.52,47.14]}]}
"#;

    #[test]
    fn local_source_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let input = dir.path().join("li.jsonl");
        std::fs::write(&input, DUMP).unwrap();

        let opts = AddOptions {
            track: true,
            ..Default::default()
        };
        let parts = import_location(&data, input.to_str().unwrap(), &opts).unwrap();
        assert_eq!(parts, vec!["li"]);
        let gen1 = geors_core::storage::read_generation(&data).unwrap();
        let reg = Registry::load(&data).unwrap();
        assert_eq!(reg.sources.len(), 1);
        assert_eq!(reg.sources[0].name, "li");

        // Unchanged file: nothing to do.
        let r = update(&data, &UpdateOptions::default()).unwrap();
        assert_eq!(r[0].outcome, Outcome::UpToDate);
        assert_eq!(geors_core::storage::read_generation(&data).unwrap(), gen1);

        // Forced: re-imported and generation bumped.
        let r = update(
            &data,
            &UpdateOptions {
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(r[0].updated());
        assert_ne!(geors_core::storage::read_generation(&data).unwrap(), gen1);

        // Changed file (different size): picked up.
        std::fs::write(&input, format!("{DUMP}\n")).unwrap();
        let r = update(&data, &UpdateOptions::default()).unwrap();
        assert!(r[0].updated());

        // A vanished file is reported, not fatal.
        std::fs::remove_file(&input).unwrap();
        let r = update(&data, &UpdateOptions::default()).unwrap();
        assert!(matches!(r[0].outcome, Outcome::Failed(_)));
        assert!(
            Registry::load(&data).unwrap().sources[0]
                .state
                .last_error
                .is_some()
        );
    }

    #[test]
    fn lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock(dir.path()).unwrap();
        assert!(lock(dir.path()).is_err());
        drop(first);
        assert!(lock(dir.path()).is_ok());
    }
}
