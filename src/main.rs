//! geors command line: `import` OSM extracts or Nominatim dumps into country
//! partitions, keep them current with `update`, `serve` them over HTTP, and
//! inspect them with `info` / `sources`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use geors_api::{ApiConfig, AppState, DataConfig};
use geors_index::EngineConfig;
use geors_index::synonyms::{SynonymConfig, Synonyms};
use geors_update::{AddOptions, Outcome, Registry, UpdateOptions};
use serde::Deserialize;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

const DEFAULT_DATA_DIR: &str = "data";
const DEFAULT_BIND: &str = "127.0.0.1:2322";

#[derive(Parser)]
#[command(
    name = "geors",
    version,
    about = "Lightweight geocoder for OpenStreetMap extracts"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Worker threads for import and search. Default: the CPUs available
    /// to the process (container CPU limits are respected).
    #[arg(long, global = true, env = "GEORS_THREADS")]
    threads: Option<usize>,
    /// Memory budget for building the text index during import, in MB
    /// (default 48 per thread, at most 4 threads; minimum 15).
    #[arg(long, global = true, env = "GEORS_INDEX_MEMORY_MB")]
    index_memory_mb: Option<usize>,
    /// How imports find way node coordinates: `memory` (random lookups,
    /// fastest when they fit in RAM), `sorted` (sequential sort-merge join,
    /// for extracts larger than RAM) or `auto`.
    #[arg(long, global = true, env = "GEORS_NODE_LOOKUP", default_value = "auto")]
    node_lookup: geors_ingest::NodeLookup,
}

#[derive(Subcommand)]
enum Command {
    /// Import extracts into per-country partitions and register them for updates.
    ///
    /// INPUT can be a local file, an http(s) URL, or `geofabrik:<region>`
    /// (e.g. `geofabrik:europe/liechtenstein`). Formats: .osm.pbf, and
    /// Nominatim/Photon JSON dumps (.json, .jsonl, .jsonl.gz).
    Import {
        #[arg(required = true, value_name = "INPUT")]
        inputs: Vec<String>,
        /// Directory holding the partitions.
        #[arg(long, env = "GEORS_DATA", default_value = DEFAULT_DATA_DIR)]
        data: PathBuf,
        /// Only import these countries (comma separated ISO codes, e.g. li,ch).
        #[arg(long, value_delimiter = ',')]
        countries: Vec<String>,
        /// Keep every country found, including small slivers across borders.
        #[arg(long, conflicts_with = "countries")]
        all_countries: bool,
        /// Country for places outside any country boundary in the extract.
        #[arg(long)]
        default_country: Option<String>,
        /// Source name used by `geors update` (default: derived from the file name).
        #[arg(long)]
        name: Option<String>,
        /// Import once without registering the source for updates.
        #[arg(long)]
        no_track: bool,
        /// Keep downloaded files in <data>/.downloads.
        #[arg(long)]
        keep_download: bool,
        /// Do not keep the extract for diff updates: `update` then always
        /// downloads it in full (saves the disk space of the extract).
        #[arg(long)]
        no_diffs: bool,
    },
    /// Check registered sources for new versions and re-import changed ones.
    ///
    /// Sources with a kept extract that publishes diffs (Geofabrik) are
    /// updated by downloading only the daily diffs and applying them.
    Update {
        #[arg(long, env = "GEORS_DATA", default_value = DEFAULT_DATA_DIR)]
        data: PathBuf,
        /// Only update these sources (comma separated names).
        #[arg(long, value_delimiter = ',')]
        sources: Vec<String>,
        /// Re-import even when nothing changed.
        #[arg(long)]
        force: bool,
        /// Download extracts in full even when diffs are available.
        #[arg(long)]
        full: bool,
        /// After a diff update, delete the local extract a source was
        /// imported from (its newer copy in <data>/.base replaces it).
        #[arg(long)]
        prune_originals: bool,
        #[arg(long)]
        keep_download: bool,
    },
    /// List registered sources, or remove one.
    Sources {
        #[arg(long, env = "GEORS_DATA", default_value = DEFAULT_DATA_DIR)]
        data: PathBuf,
        /// Stop tracking this source (its partitions are kept).
        #[arg(long, value_name = "NAME")]
        remove: Option<String>,
    },
    /// Serve the HTTP API.
    Serve {
        /// TOML config file (CLI flags take precedence).
        #[arg(long, short)]
        config: Option<PathBuf>,
        /// Directory holding the partitions.
        #[arg(long, env = "GEORS_DATA")]
        data: Option<PathBuf>,
        /// Only load these countries (comma separated). Default: all.
        #[arg(long, value_delimiter = ',')]
        countries: Vec<String>,
        /// Address to listen on.
        #[arg(long, env = "GEORS_BIND")]
        bind: Option<SocketAddr>,
        /// Run `update` inside the server at this interval, e.g. 6h, 1d.
        #[arg(long, value_name = "DURATION")]
        update_interval: Option<String>,
    },
    /// Write a partition's places as JSON lines (for inspection and diffs).
    Export {
        #[arg(long, env = "GEORS_DATA", default_value = DEFAULT_DATA_DIR)]
        data: PathBuf,
        /// Country code of the partition.
        country: String,
        /// Only every Nth place (places are in spatial order, so this is an
        /// even sample).
        #[arg(long, default_value_t = 1)]
        every: u32,
    },
    /// Show how words expand with the synonym rules (for checking rules).
    Synonyms {
        /// Words, e.g. `kerkstr cres hbf`.
        #[arg(required = true)]
        words: Vec<String>,
        /// Server config whose [synonyms] section to use (default: built-ins).
        #[arg(long, short)]
        config: Option<PathBuf>,
    },
    /// Show the partitions in a data directory.
    Info {
        #[arg(long, env = "GEORS_DATA", default_value = DEFAULT_DATA_DIR)]
        data: PathBuf,
    },
}

/// Server configuration file.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ServeConfig {
    data_dir: Option<PathBuf>,
    bind: Option<SocketAddr>,
    countries: Vec<String>,
    api: ApiConfig,
    ranking: geors_rank::RankingConfig,
    /// Abbreviation / synonym rules: built-in languages, extra rule files,
    /// inline rules (see synonyms/README.md).
    synonyms: SynonymConfig,
    updates: UpdatesConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct UpdatesConfig {
    /// Periodically run `update` inside the server.
    enabled: bool,
    /// How often to check sources for new versions.
    interval: String,
    /// How often to check the data directory for partitions changed by an
    /// external `geors import` / `geors update` ("0" disables).
    watch_interval: String,
    /// After a diff update, delete the local extract a source was imported
    /// from; the newer managed copy in `<data>/.base` replaces it.
    prune_originals: bool,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: "6h".into(),
            watch_interval: "30s".into(),
            prune_originals: false,
        }
    }
}

/// Parse `90`, `90s`, `15m`, `6h`, `1d`.
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num
        .parse()
        .with_context(|| format!("invalid duration '{s}' (e.g. 30s, 15m, 6h, 1d)"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => bail!("invalid duration unit in '{s}' (use s, m, h or d)"),
    };
    Ok(Duration::from_secs(secs))
}

/// Unix seconds -> `YYYY-MM-DD HH:MM:SSZ` (UTC).
fn iso_time(ts: u64) -> String {
    let (days, secs) = ((ts / 86_400) as i64, ts % 86_400);
    // Civil-from-days (Howard Hinnant).
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
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

fn country_codes(raw: &[String]) -> Result<Vec<String>> {
    raw.iter()
        .filter(|c| !c.trim().is_empty())
        .map(|c| {
            geors_core::normalize_country_code(c).with_context(|| {
                format!("invalid country code '{c}' (expected ISO 3166-1 alpha-2, e.g. LI)")
            })
        })
        .collect()
}

fn info_cmd(data: &Path) -> Result<()> {
    let codes = geors_index::engine::list_partitions(data)?;
    if codes.is_empty() {
        println!("no partitions in {}", data.display());
    }
    for cc in codes {
        let p = match geors_index::Partition::open(&data.join(&cc)) {
            Ok(p) => p,
            Err(e) => {
                println!("{cc}  unusable: {e}");
                continue;
            }
        };
        let m = &p.meta;
        println!(
            "{cc}  {:<20} {:>9} places  {:>8.1} MiB  source={}  bbox=[{:.4},{:.4},{:.4},{:.4}]",
            m.country_name.as_deref().unwrap_or("-"),
            m.num_places,
            dir_size(&p.dir) as f64 / 1_048_576.0,
            m.source,
            m.bbox.min_lon,
            m.bbox.min_lat,
            m.bbox.max_lon,
            m.bbox.max_lat
        );
        let layers: Vec<String> = m.layers.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!("    {}", layers.join(" "));
    }
    Ok(())
}

fn export_cmd(data: &Path, country: &str, every: u32) -> Result<()> {
    use std::io::Write;
    let cc = country_codes(&[country.to_string()])?.remove(0);
    let p = geors_index::Partition::open(&data.join(&cc))?;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for id in (0..p.len()).step_by(every.max(1) as usize) {
        let rec = p.record(id).context("corrupt record")?;
        let place = p.doc(&rec, false)?;
        let mut v = serde_json::to_value(&place)?;
        // Resolve admin indices, which are partition-specific.
        let parents: Vec<String> = place
            .parents
            .iter()
            .filter_map(|&u| p.admins.get(u as usize))
            .map(|a| format!("{}:{}", a.layer, a.name))
            .collect();
        v["parents"] = serde_json::json!(parents);
        serde_json::to_writer(&mut out, &v)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

fn synonyms_cmd(words: &[String], config: Option<PathBuf>) -> Result<()> {
    let file: ServeConfig = match &config {
        Some(path) => toml::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("invalid config file '{}'", path.display()))?,
        None => ServeConfig::default(),
    };
    let synonyms = Synonyms::load(&file.synonyms)
        .map_err(|e| anyhow::anyhow!("invalid synonym rules: {e}"))?;
    println!(
        "built-in languages: {}",
        geors_index::synonyms::builtin_languages().join(", ")
    );
    let mut analyzer = geors_index::text::analyzer();
    for word in words {
        for token in geors_index::text::tokenize(&mut analyzer, word) {
            let alts = synonyms.expand(&token);
            if alts.is_empty() {
                println!("{token}  (no expansion)");
            } else {
                println!("{token}  ->  {}", alts.join(", "));
            }
        }
    }
    Ok(())
}

fn sources_cmd(data: &Path, remove: Option<String>) -> Result<()> {
    if let Some(name) = remove {
        if !geors_update::remove_source(data, &name)? {
            bail!("no source named '{name}'");
        }
        println!("removed source '{name}' and its base file (its partitions were kept)");
        return Ok(());
    }
    let reg = Registry::load(data)?;
    if reg.sources.is_empty() {
        println!("no sources registered in {}", data.display());
    }
    let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    for s in &reg.sources {
        let v = s.state.version.clone().unwrap_or_default();
        println!("{}  [{:?}] {}", s.name, s.format, s.location);
        println!(
            "    partitions={}  data={}  sequence={}  imported={}  checked={}",
            s.state.partitions.join(","),
            opt(v.timestamp),
            opt(v.sequence.map(|n| n.to_string())),
            opt(s.state.imported_at.map(iso_time)),
            opt(s.state.checked_at.map(iso_time)),
        );
        match (&s.state.base, &s.state.replication) {
            (Some(base), Some(r)) => println!(
                "    diffs: sequence {} ({})  base={base}",
                r.sequence,
                if r.timestamp > 0 {
                    iso_time(r.timestamp as u64)
                } else {
                    "-".into()
                }
            ),
            _ if !s.diffs => println!("    diffs: disabled (--no-diffs)"),
            _ => println!("    diffs: not available (full downloads)"),
        }
        if let Some(e) = &s.state.last_error {
            println!("    last error: {e}");
        }
    }
    Ok(())
}

fn update_cmd(data: &Path, opts: &UpdateOptions) -> Result<()> {
    let reports = geors_update::update(data, opts)?;
    let mut failed = 0;
    for r in &reports {
        match &r.outcome {
            Outcome::Updated { partitions } => {
                println!("{}: updated ({})", r.name, partitions.join(","))
            }
            Outcome::UpToDate => println!("{}: up to date", r.name),
            Outcome::Failed(e) => {
                failed += 1;
                println!("{}: FAILED: {e}", r.name);
            }
        }
    }
    if failed > 0 {
        bail!("{failed} source(s) failed to update");
    }
    Ok(())
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Background loop: run `update`, then hot-reload if anything changed.
fn spawn_updater(state: Arc<AppState>, every: Duration, opts: UpdateOptions) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let s = state.clone();
            let opts = opts.clone();
            let result = tokio::task::spawn_blocking(move || {
                let reports = geors_update::update(&s.data.data_dir, &opts)?;
                if reports.iter().any(|r| r.updated()) {
                    s.reload_if_changed()?;
                }
                anyhow::Ok(reports)
            })
            .await;
            match result {
                Ok(Ok(reports)) => {
                    let updated: Vec<&str> = reports
                        .iter()
                        .filter(|r| r.updated())
                        .map(|r| r.name.as_str())
                        .collect();
                    info!(checked = reports.len(), updated = %updated.join(","), next_in = ?every, "update check finished");
                }
                Ok(Err(e)) => warn!(error = %format!("{e:#}"), "update check failed; will retry"),
                Err(e) => error!(error = %e, "update task panicked"),
            }
        }
    });
}

async fn serve(
    config: Option<PathBuf>,
    data: Option<PathBuf>,
    countries: Vec<String>,
    bind: Option<SocketAddr>,
    update_interval: Option<String>,
) -> Result<()> {
    let file: ServeConfig = match &config {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read config file '{}'", path.display()))?;
            toml::from_str(&text)
                .with_context(|| format!("invalid config file '{}'", path.display()))?
        }
        None => ServeConfig::default(),
    };
    let data_dir = data
        .or(file.data_dir)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));
    let countries = country_codes(if countries.is_empty() {
        &file.countries
    } else {
        &countries
    })?;
    let bind = bind
        .or(file.bind)
        .unwrap_or_else(|| DEFAULT_BIND.parse().unwrap());
    let update_every = match update_interval {
        Some(i) => Some(parse_duration(&i)?),
        None if file.updates.enabled => Some(parse_duration(&file.updates.interval)?),
        None => None,
    };
    if update_every.is_some_and(|d| d < Duration::from_secs(60)) {
        bail!("update interval must be at least 60s (extracts are published daily at most)");
    }
    let watch_every = parse_duration(&file.updates.watch_interval)?;

    let t = Instant::now();
    let engine = EngineConfig {
        ranking: file.ranking,
        synonyms: Synonyms::load(&file.synonyms)
            .map_err(|e| anyhow::anyhow!("invalid synonym rules: {e}"))?,
    };
    let data = DataConfig {
        data_dir,
        countries,
        engine,
    };
    let state = Arc::new(AppState::open(data, file.api)?);
    info!(partitions = state.engine().partitions().len(), elapsed = ?t.elapsed(), "engine ready");

    if !watch_every.is_zero() {
        geors_api::spawn_watcher(state.clone(), watch_every);
    }
    if let Some(every) = update_every {
        info!(every = ?every, "automatic updates enabled");
        let opts = UpdateOptions {
            prune_originals: file.updates.prune_originals,
            ..Default::default()
        };
        spawn_updater(state.clone(), every, opts);
    }

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot listen on {bind}"))?;
    info!("listening on http://{bind}");
    geors_api::serve(listener, state, async {
        let _ = tokio::signal::ctrl_c().await;
        info!("shutting down");
    })
    .await?;
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tantivy=warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let threads = cli
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .max(1);
    // One pool size for everything CPU-bound: PBF decoding, index writing
    // (rayon) and the tantivy indexer derive from it.
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .context("cannot configure thread pool")?;
    geors_ingest::set_default_node_lookup(cli.node_lookup);
    // Spill files up to a quarter of the memory are read in place while
    // writing; larger ones are regrouped for sequential reads.
    let memory = geors_ingest::memory_limit();
    geors_index::writer::set_writer_memory(
        memory.map_or(u64::MAX, |m| m / 4),
        geors_ingest::extsort::buffer_bytes() / 2,
    );
    match (cli.index_memory_mb, memory) {
        (Some(mb), _) => geors_index::text::set_index_memory(mb * 1024 * 1024),
        // Default: 48 MB per indexing thread, at most an eighth of memory.
        (None, Some(m)) => geors_index::text::set_index_memory(
            ((m / 8) as usize).min(threads.min(4) * (48 << 20)),
        ),
        (None, None) => {}
    }
    match cli.command {
        Command::Import {
            inputs,
            data,
            countries,
            all_countries,
            default_country,
            name,
            no_track,
            keep_download,
            no_diffs,
        } => {
            if name.is_some() && inputs.len() > 1 {
                bail!("--name can only be used with a single input");
            }
            let opts = AddOptions {
                name,
                countries: country_codes(&countries)?,
                all_countries,
                default_country: default_country
                    .map(|c| country_codes(&[c]))
                    .transpose()?
                    .and_then(|v| v.into_iter().next()),
                track: !no_track,
                keep_download,
                no_diffs,
            };
            for input in &inputs {
                geors_update::import_location(&data, input, &opts)?;
            }
            Ok(())
        }
        Command::Update {
            data,
            sources,
            force,
            full,
            prune_originals,
            keep_download,
        } => update_cmd(
            &data,
            &UpdateOptions {
                force,
                full,
                sources,
                keep_download,
                prune_originals,
            },
        ),
        Command::Sources { data, remove } => sources_cmd(&data, remove),
        Command::Info { data } => info_cmd(&data),
        Command::Synonyms { words, config } => synonyms_cmd(&words, config),
        Command::Export {
            data,
            country,
            every,
        } => export_cmd(&data, &country, every),
        Command::Serve {
            config,
            data,
            countries,
            bind,
            update_interval,
        } => tokio::runtime::Builder::new_multi_thread()
            // Network I/O needs few threads; searches run on the blocking
            // pool, capped at `threads` so concurrent requests queue instead
            // of oversubscribing the CPUs (tokio's default cap is 512).
            .worker_threads(threads.min(2))
            .max_blocking_threads(threads)
            .enable_all()
            .build()?
            .block_on(serve(config, data, countries, bind, update_interval)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(21_600));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
        assert!(parse_duration("6x").is_err());
        assert!(parse_duration("h").is_err());
    }

    #[test]
    fn iso_times() {
        assert_eq!(iso_time(0), "1970-01-01 00:00:00Z");
        assert_eq!(iso_time(1_791_530_052), "2026-10-09 07:14:12Z");
        assert_eq!(iso_time(951_782_400), "2000-02-29 00:00:00Z");
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../geors.example.toml");
        let cfg: ServeConfig = toml::from_str(text).unwrap();
        assert!(!cfg.updates.enabled);
        assert!(cfg.synonyms.words.contains_key("kh"));
        assert!(Synonyms::load(&cfg.synonyms).is_ok());
    }
}
