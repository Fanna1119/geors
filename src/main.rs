//! geors command line: `import` OSM extracts into country partitions, `serve`
//! them over HTTP, and inspect them with `info`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use geors_api::{ApiConfig, AppState};
use geors_index::{Engine, PartitionInput, write_partition};
use geors_ingest::ImportOptions;
use serde::Deserialize;
use tracing::info;
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
    /// Import one or more .osm.pbf extracts into per-country partitions.
    Import {
        /// OSM PBF file(s), e.g. liechtenstein-latest.osm.pbf
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
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

fn import(
    inputs: &[PathBuf],
    data: &Path,
    countries: &[String],
    all_countries: bool,
    default_country: Option<&str>,
) -> Result<()> {
    let opts = ImportOptions {
        countries: country_codes(countries)?,
        all_countries,
        default_country: default_country
            .map(|c| country_codes(&[c.to_string()]))
            .transpose()?
            .and_then(|v| v.into_iter().next()),
        ..Default::default()
    };
    for input in inputs {
        if !input.is_file() {
            bail!("input file '{}' does not exist", input.display());
        }
        let t = Instant::now();
        info!(input = %input.display(), "importing");
        let countries = geors_ingest::import_pbf(input, &opts)?;
        if countries.is_empty() {
            bail!(
                "no places imported from '{}'; if the extract has no country boundary, pass --default-country",
                input.display()
            );
        }
        let source = input
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        for c in countries {
            let input = PartitionInput {
                country_code: c.country_code,
                country_name: c.country_name,
                source: source.clone(),
                places: c.places,
                admins: c.admins,
            };
            write_partition(data, input)?;
        }
        info!(input = %source, elapsed = ?t.elapsed(), "import finished");
    }
    Ok(())
}

fn info_cmd(data: &Path) -> Result<()> {
    let codes = geors_index::engine::list_partitions(data)?;
    if codes.is_empty() {
        println!("no partitions in {}", data.display());
    }
    for cc in codes {
        let p = geors_index::Partition::open(&data.join(&cc))?;
        let m = &p.meta;
        let size: u64 = dir_size(&p.dir);
        println!(
            "{cc}  {:<20} {:>9} places  {:>8.1} MiB  source={}  bbox=[{:.4},{:.4},{:.4},{:.4}]",
            m.country_name.as_deref().unwrap_or("-"),
            m.num_places,
            size as f64 / 1_048_576.0,
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

async fn serve(
    config: Option<PathBuf>,
    data: Option<PathBuf>,
    countries: Vec<String>,
    bind: Option<SocketAddr>,
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
    let data = data
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

    let t = Instant::now();
    let engine = Engine::open(&data, &countries, file.ranking)?;
    info!(partitions = engine.partitions().len(), elapsed = ?t.elapsed(), "engine ready");

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot listen on {bind}"))?;
    info!("listening on http://{bind}");
    let state = Arc::new(AppState {
        engine,
        config: file.api,
    });
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
        .init();
    match Cli::parse().command {
        Command::Import {
            inputs,
            data,
            countries,
            all_countries,
            default_country,
        } => import(
            &inputs,
            &data,
            &countries,
            all_countries,
            default_country.as_deref(),
        ),
        Command::Info { data } => info_cmd(&data),
        Command::Serve {
            config,
            data,
            countries,
            bind,
        } => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(serve(config, data, countries, bind)),
    }
}
