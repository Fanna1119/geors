//! geors command line: `import` OSM extracts or Nominatim dumps into country
//! partitions, keep them current with `update`, `serve` them over HTTP, and
//! inspect them with `info` / `sources`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use geors_api::{ApiConfig, AppState, DataConfig};
use geors_index::EngineConfig;
use geors_index::synonyms::Synonyms;
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
    },
    /// Check registered sources for new versions and re-import changed ones.
    Update {
        #[arg(long, env = "GEORS_DATA", default_value = DEFAULT_DATA_DIR)]
        data: PathBuf,
        /// Only update these sources (comma separated names).
        #[arg(long, value_delimiter = ',')]
        sources: Vec<String>,
        /// Re-import even when nothing changed.
        #[arg(long)]
        force: bool,
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
    /// Extra abbreviations / synonyms, e.g. `"bhf" = ["bahnhof"]`, `"*gs" = ["gasse"]`.
    synonyms: BTreeMap<String, Vec<String>>,
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
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: "6h".into(),
            watch_interval: "30s".into(),
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
        let p = geors_index::Partition::open(&data.join(&cc))?;
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

fn sources_cmd(data: &Path, remove: Option<String>) -> Result<()> {
    let mut reg = Registry::load(data)?;
    if let Some(name) = remove {
        let _lock = geors_update::lock(data)?;
        if reg.remove(&name).is_none() {
            bail!("no source named '{name}'");
        }
        reg.save()?;
        println!("removed source '{name}' (its partitions were kept)");
        return Ok(());
    }
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
fn spawn_updater(state: Arc<AppState>, every: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let s = state.clone();
            let result = tokio::task::spawn_blocking(move || {
                let reports = geors_update::update(&s.data.data_dir, &UpdateOptions::default())?;
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
        synonyms: Synonyms::new(&file.synonyms),
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
        spawn_updater(state.clone(), every);
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
    match Cli::parse().command {
        Command::Import {
            inputs,
            data,
            countries,
            all_countries,
            default_country,
            name,
            no_track,
            keep_download,
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
            keep_download,
        } => update_cmd(
            &data,
            &UpdateOptions {
                force,
                sources,
                keep_download,
            },
        ),
        Command::Sources { data, remove } => sources_cmd(&data, remove),
        Command::Info { data } => info_cmd(&data),
        Command::Serve {
            config,
            data,
            countries,
            bind,
            update_interval,
        } => tokio::runtime::Builder::new_multi_thread()
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
        assert!(cfg.synonyms.contains_key("bhf"));
    }
}
