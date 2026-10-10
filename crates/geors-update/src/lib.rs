//! Data management: importing extracts (local files or URLs), remembering
//! where they came from, and keeping them up to date.
//!
//! Every import registers a [`Source`] in `<data>/sources.json`. `update`
//! checks each source for a newer published version (Geofabrik replication
//! sequence, MD5, ETag, or file mtime), downloads and re-imports changed ones,
//! and bumps `<data>/GENERATION` so running servers hot-reload.

pub mod osc;
pub mod osm;
pub mod pbf_write;
pub mod remote;
pub mod replicate;
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
const BASE_DIR: &str = ".base";

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
///
/// With `discard_input` the input file is deleted as soon as it has been
/// read, before the partitions are written, so a large download and the
/// finished index never need disk space at the same time.
pub fn import_file(
    path: &Path,
    format: Format,
    data_dir: &Path,
    opts: &ImportOptions,
    source_label: &str,
    discard_input: bool,
) -> Result<Vec<String>> {
    let t = Instant::now();
    info!(input = %path.display(), ?format, "importing");
    // Scratch files go next to the data (usually the roomiest disk).
    let mut opts = opts.clone();
    if opts.work_dir.is_none() {
        std::fs::create_dir_all(data_dir)?;
        opts.work_dir = Some(data_dir.to_path_buf());
    }
    let import = match format {
        Format::Pbf => geors_ingest::import_pbf(path, &opts)?,
        Format::Dump => geors_ingest::dump::import_dump(path, &opts)?,
    };
    if discard_input {
        std::fs::remove_file(path)
            .with_context(|| format!("cannot remove '{}'", path.display()))?;
    }
    if import.countries.is_empty() {
        bail!(
            "no places imported from '{}'; if the extract has no country boundary, pass --default-country",
            path.display()
        );
    }
    let mut written = Vec::new();
    for c in &import.countries {
        let code = c.country_code.clone();
        write_partition(
            data_dir,
            PartitionInput {
                country_code: c.country_code.clone(),
                country_name: c.country_name.clone(),
                source: source_label.to_string(),
                places: c.places.clone(),
                admins: c.admins.clone(),
            },
        )?;
        written.push(code);
    }
    drop(import); // removes the scratch files
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
    /// Do not keep the PBF for diff updates; always download in full.
    pub no_diffs: bool,
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
    clean_data_dir(data_dir, &Registry::load(data_dir)?);
    let mut source = Source {
        name: opts.name.clone().unwrap_or_else(|| default_name(&location)),
        location,
        format,
        countries: opts.countries.clone(),
        all_countries: opts.all_countries,
        default_country: opts.default_country.clone(),
        diffs: !opts.no_diffs,
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
        // With diffs, the download becomes the base the diffs are applied to.
        let replication = replication_of(source, &dest);
        let (path, discard) = match &replication {
            Some(_) => {
                let base = base_path(data_dir, &source.name);
                std::fs::create_dir_all(base.parent().unwrap())?;
                std::fs::rename(&dest, &base)?;
                (base, false)
            }
            None => (dest, !keep_download),
        };
        let partitions = import_file(&path, source.format, data_dir, &opts, &label, discard)?;
        set_base(source, replication.map(|r| (path, r)));
        (partitions, version)
    } else {
        let path = Path::new(&source.location);
        let version = remote::check_local(path)?;
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let partitions = import_file(path, source.format, data_dir, &opts, &label, false)?;
        // A local Geofabrik extract can be kept current with diffs too; the
        // user's file itself is never modified (see `diff_update`).
        set_base(
            source,
            replication_of(source, path).map(|r| (path.to_path_buf(), r)),
        );
        (partitions, version)
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

/// Where the base PBF of a source with diff updates is kept.
fn base_path(data_dir: &Path, name: &str) -> PathBuf {
    data_dir.join(BASE_DIR).join(format!("{name}.osm.pbf"))
}

/// Replication info of `path` if diffs are enabled and the file has it.
fn replication_of(source: &Source, path: &Path) -> Option<pbf_write::Replication> {
    if source.format != Format::Pbf || !source.diffs {
        return None;
    }
    match replicate::read_replication(path) {
        Ok(r) => r,
        Err(e) => {
            warn!(source = %source.name, error = %format!("{e:#}"), "cannot read replication header; diffs disabled");
            None
        }
    }
}

fn set_base(source: &mut Source, base: Option<(PathBuf, pbf_write::Replication)>) {
    match base {
        Some((path, r)) => {
            info!(source = %source.name, sequence = r.sequence, "diff updates enabled");
            // Absolute, so updates work from any working directory (cron).
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            source.state.base = Some(path.to_string_lossy().to_string());
            source.state.replication = Some(r.into());
        }
        None => {
            source.state.base = None;
            source.state.replication = None;
        }
    }
}

/// Diffs are kept by Geofabrik for 100 days; beyond this many a full
/// download is also simply faster.
const MAX_DIFFS: u64 = 90;

/// Bring a source with a base PBF up to date by applying diffs, then
/// re-import. `Ok(None)` means diffs cannot be used and the caller should
/// do a full update instead.
fn diff_update(
    http: &Http,
    data_dir: &Path,
    source: &mut Source,
    force: bool,
    prune_original: bool,
) -> Result<Option<Outcome>> {
    let (Some(base), Some(rep)) = (source.state.base.clone(), source.state.replication.clone())
    else {
        return Ok(None);
    };
    let base = PathBuf::from(base);
    if !source.diffs || !base.is_file() {
        return Ok(None);
    }
    // A local source whose file was replaced by a newer extract: use that.
    if !source.is_remote()
        && let Some(local) = replication_of(source, Path::new(&source.location))
        && local.sequence > rep.sequence
    {
        return Ok(None);
    }
    let (latest, timestamp) = replicate::latest_state(http, &rep.base_url)?;
    if latest <= rep.sequence && !force {
        info!(source = %source.name, sequence = rep.sequence, "up to date");
        return Ok(Some(Outcome::UpToDate));
    }
    if latest > rep.sequence + MAX_DIFFS {
        info!(source = %source.name, behind = latest - rep.sequence, "too far behind for diffs; downloading in full");
        return Ok(None);
    }
    let managed = base_path(data_dir, &source.name);
    let path = if latest > rep.sequence {
        info!(source = %source.name, from = rep.sequence, to = latest, "applying diffs");
        let changes = replicate::fetch_changes(http, &rep.base_url, rep.sequence, latest)?;
        std::fs::create_dir_all(managed.parent().unwrap())?;
        let tmp = managed.with_extension("pbf.tmp");
        let new_rep = pbf_write::Replication {
            base_url: rep.base_url.clone(),
            sequence: latest,
            timestamp,
        };
        replicate::apply_changes(&base, changes, &tmp, &new_rep)?;
        std::fs::rename(&tmp, &managed)?;
        managed.clone()
    } else {
        base
    };
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let partitions = import_file(
        &path,
        source.format,
        data_dir,
        &source.import_options(),
        &label,
        false,
    )?;
    let rep = replicate::read_replication(&path)?.context("base lost its replication header")?;
    let iso = (rep.timestamp > 0).then(|| unix_to_iso(rep.timestamp));
    source.state.version = Some(Version {
        sequence: Some(rep.sequence),
        timestamp: iso,
        ..Default::default()
    });
    source.state.partitions = partitions.clone();
    source.state.imported_at = Some(now());
    source.state.last_error = None;
    let sequence = rep.sequence;
    set_base(source, Some((path.clone(), rep)));
    if prune_original && !source.is_remote() && path == managed {
        prune_original_file(source, &managed, sequence);
    }
    Ok(Some(Outcome::Updated { partitions }))
}

/// Delete the local extract a source was imported from once the managed
/// base has strictly newer data, and point the source at the managed base.
/// Files without replication info, or as new as the base, are kept.
fn prune_original_file(source: &mut Source, managed: &Path, managed_seq: u64) {
    let original = PathBuf::from(&source.location);
    let managed = std::fs::canonicalize(managed).unwrap_or_else(|_| managed.to_path_buf());
    if std::fs::canonicalize(&original).is_ok_and(|o| o == managed) {
        return;
    }
    if original.exists() {
        match replicate::read_replication(&original) {
            Ok(Some(r)) if r.sequence < managed_seq => {}
            _ => return,
        }
        let size = std::fs::metadata(&original).map(|m| m.len()).unwrap_or(0);
        if let Err(e) = std::fs::remove_file(&original) {
            warn!(file = %original.display(), error = %e, "cannot delete superseded extract");
            return;
        }
        info!(
            source = %source.name,
            file = %original.display(),
            freed_mb = size / 1_048_576,
            "deleted superseded extract; source now uses its managed copy"
        );
    }
    source.location = managed.to_string_lossy().to_string();
}

fn size_of(path: &Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| size_of(&e.path()))
            .sum(),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Remove geors' own leftovers in the data directory: scratch directories
/// and half-written files of interrupted imports, and base files of sources
/// that are no longer registered. Only call while holding the [`DataLock`]
/// (no other import can be using them). Returns the bytes freed.
pub fn clean_data_dir(data_dir: &Path, registry: &Registry) -> u64 {
    let mut doomed: Vec<PathBuf> = Vec::new();
    let is_scratch = |name: &str| {
        (name.starts_with('.')
            && (name.starts_with(".geors-import-")
                || name.contains(".tmp-")
                || name.contains(".old-")
                || name.contains(".spill-")))
            || name.starts_with("sources.json.tmp-")
    };
    for entry in std::fs::read_dir(data_dir).into_iter().flatten().flatten() {
        if is_scratch(&entry.file_name().to_string_lossy()) {
            doomed.push(entry.path());
        }
    }
    let wanted: Vec<PathBuf> = registry
        .sources
        .iter()
        .map(|s| base_path(data_dir, &s.name))
        .collect();
    for entry in std::fs::read_dir(data_dir.join(BASE_DIR))
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".tmp") || (name.ends_with(".osm.pbf") && !wanted.contains(&path)) {
            doomed.push(path);
        }
    }
    for entry in std::fs::read_dir(data_dir.join(DOWNLOAD_DIR))
        .into_iter()
        .flatten()
        .flatten()
    {
        if entry.file_name().to_string_lossy().ends_with(".part") {
            doomed.push(entry.path());
        }
    }
    let mut freed = 0;
    for path in doomed {
        let size = size_of(&path);
        match remove_any(&path) {
            Ok(()) => {
                info!(path = %path.display(), mb = size / 1_048_576, "removed leftover");
                freed += size;
            }
            Err(e) => warn!(path = %path.display(), error = %e, "cannot remove leftover"),
        }
    }
    freed
}

/// Stop tracking a source and delete its managed base file. Its partitions
/// are kept. Returns false if there is no such source.
pub fn remove_source(data_dir: &Path, name: &str) -> Result<bool> {
    let _lock = lock(data_dir)?;
    let mut reg = Registry::load(data_dir)?;
    if reg.remove(name).is_none() {
        return Ok(false);
    }
    reg.save()?;
    clean_data_dir(data_dir, &reg);
    Ok(true)
}

/// Unix seconds -> `YYYY-MM-DDTHH:MM:SSZ`.
fn unix_to_iso(ts: i64) -> String {
    let (days, secs) = (ts.div_euclid(86_400), ts.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    /// Re-import even if the source looks unchanged.
    pub force: bool,
    /// Download sources in full even when diffs are available.
    pub full: bool,
    /// Only these sources (by name). Empty = all.
    pub sources: Vec<String>,
    pub keep_download: bool,
    /// After a diff update of a source imported from a local file, delete
    /// that file: the newer managed copy in `<data>/.base` replaces it.
    pub prune_originals: bool,
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
    clean_data_dir(data_dir, &reg);
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
        let diffed = if opts.full {
            Ok(None)
        } else {
            diff_update(&http, data_dir, source, opts.force, opts.prune_originals)
        };
        let outcome = match diffed {
            Ok(Some(outcome)) => outcome,
            Err(e) if !source.is_remote() => Outcome::Failed(format!("{e:#}")),
            other => {
                if let Err(e) = other {
                    warn!(source = %name, error = %format!("{e:#}"), "diff update failed; downloading in full");
                }
                full_update(&http, data_dir, source, opts)
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

/// Update by checking the published version and re-downloading if needed.
fn full_update(http: &Http, data_dir: &Path, source: &mut Source, opts: &UpdateOptions) -> Outcome {
    let current = match check(http, source) {
        Ok(v) => v,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    let unchanged = source
        .state
        .version
        .as_ref()
        .is_some_and(|v| !current.differs_from(v));
    if unchanged && !opts.force && !opts.full {
        // Sources registered before diff support: start using the local
        // extract as the diff base from now on.
        if source.state.base.is_none() && !source.is_remote() {
            let path = PathBuf::from(&source.location);
            set_base(source, replication_of(source, &path).map(|r| (path, r)));
        }
        info!(source = %source.name, timestamp = current.timestamp.as_deref().unwrap_or("-"), "up to date");
        return Outcome::UpToDate;
    }
    info!(source = %source.name, sequence = ?current.sequence, timestamp = current.timestamp.as_deref().unwrap_or("-"), "new version available");
    match fetch_and_import(http, data_dir, source, Some(current), opts.keep_download) {
        Ok(partitions) => Outcome::Updated { partitions },
        Err(e) => Outcome::Failed(format!("{e:#}")),
    }
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

    fn pbf(path: &Path, seq: Option<u64>) {
        let rep = seq.map(|sequence| pbf_write::Replication {
            base_url: "https://example.org/x-updates".into(),
            sequence,
            timestamp: 0,
        });
        std::fs::write(
            path,
            pbf_write::blob("OSMHeader", &pbf_write::header_block(rep.as_ref())).unwrap(),
        )
        .unwrap();
    }

    fn local_source(location: &Path) -> Source {
        Source {
            name: "x".into(),
            location: location.to_string_lossy().to_string(),
            format: Format::Pbf,
            countries: vec![],
            all_countries: false,
            default_country: None,
            diffs: true,
            state: SourceState::default(),
        }
    }

    #[test]
    fn prunes_only_superseded_originals() {
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed.osm.pbf");
        pbf(&managed, Some(6));
        let original = dir.path().join("x-latest.osm.pbf");

        // Older than the managed base: deleted, source re-pointed.
        pbf(&original, Some(5));
        let mut s = local_source(&original);
        prune_original_file(&mut s, &managed, 6);
        assert!(!original.exists());
        assert_eq!(
            PathBuf::from(&s.location),
            std::fs::canonicalize(&managed).unwrap()
        );

        // As new as the base (the user dropped in a fresh file): kept.
        pbf(&original, Some(6));
        let mut s = local_source(&original);
        prune_original_file(&mut s, &managed, 6);
        assert!(original.exists());

        // No replication header: not ours to judge, kept.
        pbf(&original, None);
        let mut s = local_source(&original);
        prune_original_file(&mut s, &managed, 6);
        assert!(original.exists());
    }

    #[test]
    fn cleans_only_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        for d in [
            ".geors-import-1-0",
            ".at.tmp-12",
            ".at.old-12",
            ".li.spill-3-1",
            "at",
            ".base",
            ".downloads",
        ] {
            std::fs::create_dir_all(data.join(d)).unwrap();
        }
        std::fs::write(data.join("at/meta.json"), "{}").unwrap();
        std::fs::write(data.join("sources.json.tmp-9"), "x").unwrap();
        std::fs::write(data.join(".base/kept.osm.pbf"), "x").unwrap();
        std::fs::write(data.join(".base/orphan.osm.pbf"), "xx").unwrap();
        std::fs::write(data.join(".base/kept.pbf.tmp"), "x").unwrap();
        std::fs::write(data.join(".downloads/a.osm.part"), "x").unwrap();
        std::fs::write(data.join(".lock"), "").unwrap();
        let mut reg = Registry::load(data).unwrap();
        let mut s = local_source(Path::new("/nowhere.osm.pbf"));
        s.name = "kept".into();
        reg.upsert(s);
        let freed = clean_data_dir(data, &reg);
        assert!(freed >= 5);
        let mut left: Vec<String> = walk(data);
        left.sort();
        assert_eq!(
            left,
            vec![
                ".base",
                ".base/kept.osm.pbf",
                ".downloads",
                ".lock",
                "at",
                "at/meta.json"
            ]
        );
    }

    fn walk(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(root).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() {
                for c in std::fs::read_dir(e.path()).unwrap().flatten() {
                    out.push(format!("{name}/{}", c.file_name().to_string_lossy()));
                }
            }
            out.push(name);
        }
        out
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
