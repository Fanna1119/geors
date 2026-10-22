use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use geors_core::storage::read_generation;
use geors_core::{BBox, LonLat};
use geors_index::{Engine, EngineConfig, Hit, PlaceRef};
use geors_rank::RankingConfig;
use tracing::info;

use crate::request::{Contains, Nearest, Reverse, Search};
use crate::{Result, Synonyms};

/// How to open a data directory.
#[derive(Debug, Clone)]
pub struct GeocoderConfig {
    /// Only these countries (lowercase ISO codes); empty: every partition.
    pub countries: Vec<String>,
    /// Ranking weights (see [`RankingConfig`]).
    pub ranking: RankingConfig,
    /// Abbreviation and synonym rules (default: the built-in ones).
    pub synonyms: Synonyms,
}

impl Default for GeocoderConfig {
    fn default() -> Self {
        let engine = EngineConfig::default();
        Self {
            countries: Vec::new(),
            ranking: engine.ranking,
            synonyms: engine.synonyms,
        }
    }
}

impl GeocoderConfig {
    fn engine(&self) -> EngineConfig {
        EngineConfig {
            ranking: self.ranking.clone(),
            synonyms: self.synonyms.clone(),
        }
    }
}

/// One loaded partition (a country, or a further part of one).
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    /// Directory name: the country code, or `<cc>@<source>`.
    pub name: String,
    /// Lowercase ISO code.
    pub country_code: String,
    pub country_name: Option<String>,
    pub places: u32,
    /// Places per layer (`"city"`, `"street"`, ...).
    pub layers: BTreeMap<String, u32>,
    pub bbox: BBox,
    /// File the partition was imported from.
    pub source: String,
    /// Unix seconds of the import.
    pub created_unix: u64,
}

/// What is loaded and since when.
#[derive(Debug, Clone)]
pub struct Status {
    pub data_dir: PathBuf,
    /// `<data>/GENERATION` when the data was loaded (changes with every
    /// import or update).
    pub generation: Option<String>,
    /// Unix seconds of the last (re)load.
    pub loaded_unix: u64,
    pub partitions: Vec<PartitionInfo>,
}

struct Loaded {
    generation: Option<String>,
    unix: u64,
}

/// A geocoder over the partitions in one data directory.
///
/// Cheap to share (`Arc<Geocoder>`; it is `Send + Sync`). Queries are
/// synchronous and use the rayon pool internally; from async code, call
/// them in `spawn_blocking`. Imports and updates replace partitions
/// atomically: [`Geocoder::reload_if_changed`] picks them up while queries
/// in flight finish on the old data.
pub struct Geocoder {
    data_dir: PathBuf,
    config: GeocoderConfig,
    engine: ArcSwap<Engine>,
    loaded: Mutex<Loaded>,
    /// Serialises reloads.
    reload_lock: Mutex<()>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Geocoder {
    /// Open the partitions in `data_dir` (written by `geors import`).
    pub fn open(data_dir: impl AsRef<Path>, config: GeocoderConfig) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let generation = read_generation(&data_dir);
        let engine = Engine::open(&data_dir, &config.countries, config.engine())?;
        Ok(Self {
            data_dir,
            config,
            engine: ArcSwap::from_pointee(engine),
            loaded: Mutex::new(Loaded {
                generation,
                unix: now(),
            }),
            reload_lock: Mutex::new(()),
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config(&self) -> &GeocoderConfig {
        &self.config
    }

    /// Text search (see [`Search`]).
    pub fn search(&self, search: &Search) -> Result<Vec<Hit>> {
        Ok(self.engine.load().search(&search.build()?)?)
    }

    /// Reverse geocoding (see [`Reverse`]).
    pub fn reverse(&self, reverse: &Reverse) -> Result<Vec<Hit>> {
        Ok(self.engine.load().nearest(&reverse.build()?)?)
    }

    /// The nearest places (see [`Nearest`]).
    pub fn nearest(&self, nearest: &Nearest) -> Result<Vec<Hit>> {
        Ok(self.engine.load().nearest(&nearest.build()?)?)
    }

    /// Places by OSM id (`"N123".parse()`, `"W456"`, `"R789"`) or Nominatim
    /// `place_id` (`"P123"`), in request order; unknown ids are skipped.
    /// A merged street is found by the id of any of its OSM ways.
    /// `countries` (lowercase ISO codes) restricts where to look.
    pub fn lookup(&self, ids: &[PlaceRef], countries: &[String]) -> Result<Vec<Hit>> {
        Ok(self.engine.load().lookup(ids, countries)?)
    }

    /// Areas containing a point (see [`Contains`]).
    pub fn contains(&self, contains: &Contains) -> Result<Vec<Hit>> {
        let (point, countries, layers): (LonLat, _, _) = contains.build()?;
        Ok(self.engine.load().containing(point, &countries, &layers)?)
    }

    /// What is loaded.
    pub fn status(&self) -> Status {
        let engine = self.engine.load();
        let loaded = self.loaded.lock().unwrap();
        Status {
            data_dir: self.data_dir.clone(),
            generation: loaded.generation.clone(),
            loaded_unix: loaded.unix,
            partitions: engine
                .partitions()
                .iter()
                .map(|p| {
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
                })
                .collect(),
        }
    }

    /// Reopen the partitions if they changed on disk (`<data>/GENERATION`,
    /// bumped by every import and update). Blocking; returns whether new
    /// data was swapped in.
    pub fn reload_if_changed(&self) -> Result<bool> {
        let _guard = self.reload_lock.lock().unwrap();
        // Read the marker before opening: a change during the open
        // triggers another reload next time.
        let generation = read_generation(&self.data_dir);
        if generation == self.loaded.lock().unwrap().generation {
            return Ok(false);
        }
        let t = Instant::now();
        let engine = Engine::open(&self.data_dir, &self.config.countries, self.config.engine())?;
        let partitions: Vec<String> = engine
            .partitions()
            .iter()
            .map(|p| format!("{}={}", p.name(), p.len()))
            .collect();
        self.engine.store(Arc::new(engine));
        *self.loaded.lock().unwrap() = Loaded {
            generation,
            unix: now(),
        };
        info!(partitions = %partitions.join(" "), elapsed = ?t.elapsed(), "reloaded partitions");
        Ok(true)
    }
}
