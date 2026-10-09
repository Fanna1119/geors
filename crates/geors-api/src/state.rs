//! Shared server state with a hot-swappable engine.
//!
//! Imports and updates replace partition directories atomically and bump
//! `<data>/GENERATION`. [`AppState::reload_if_changed`] notices that, opens
//! a fresh engine and swaps it in; requests in flight keep using the old one
//! until they finish (its memory maps stay valid after the files are replaced).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use geors_core::storage::read_generation;
use geors_index::{Engine, EngineConfig, IndexError};
use tracing::{error, info};

use crate::ApiConfig;

/// What to load, so the engine can be reopened later.
#[derive(Debug, Clone)]
pub struct DataConfig {
    pub data_dir: PathBuf,
    /// Lowercase country codes; empty = every partition in `data_dir`.
    pub countries: Vec<String>,
    pub engine: EngineConfig,
}

#[derive(Debug, Clone, Default)]
pub struct LoadInfo {
    pub generation: Option<String>,
    pub loaded_unix: u64,
}

pub struct AppState {
    engine: ArcSwap<Engine>,
    pub config: ApiConfig,
    pub data: DataConfig,
    loaded: Mutex<LoadInfo>,
    /// Serialises reloads.
    reload_lock: Mutex<()>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl AppState {
    pub fn open(data: DataConfig, config: ApiConfig) -> Result<Self, IndexError> {
        let generation = read_generation(&data.data_dir);
        let engine = Engine::open(&data.data_dir, &data.countries, data.engine.clone())?;
        Ok(Self {
            engine: ArcSwap::from_pointee(engine),
            config,
            data,
            loaded: Mutex::new(LoadInfo {
                generation,
                loaded_unix: now(),
            }),
            reload_lock: Mutex::new(()),
        })
    }

    /// The current engine. Cheap; hold it for the duration of one request.
    pub fn engine(&self) -> Arc<Engine> {
        self.engine.load_full()
    }

    pub fn load_info(&self) -> LoadInfo {
        self.loaded.lock().unwrap().clone()
    }

    /// Reopen the engine if partitions changed on disk. Blocking.
    /// Returns whether a new engine was swapped in.
    pub fn reload_if_changed(&self) -> Result<bool, IndexError> {
        let _guard = self.reload_lock.lock().unwrap();
        // Read the marker *before* opening: a change during the open
        // triggers another reload next time.
        let generation = read_generation(&self.data.data_dir);
        if generation == self.loaded.lock().unwrap().generation {
            return Ok(false);
        }
        let t = Instant::now();
        let engine = Engine::open(
            &self.data.data_dir,
            &self.data.countries,
            self.data.engine.clone(),
        )?;
        let partitions: Vec<String> = engine
            .partitions()
            .iter()
            .map(|p| format!("{}={}", p.code(), p.len()))
            .collect();
        self.engine.store(Arc::new(engine));
        *self.loaded.lock().unwrap() = LoadInfo {
            generation,
            loaded_unix: now(),
        };
        info!(partitions = %partitions.join(" "), elapsed = ?t.elapsed(), "reloaded partitions");
        Ok(true)
    }
}

/// Poll `<data>/GENERATION` and hot-reload when it changes.
pub fn spawn_watcher(state: Arc<AppState>, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            let s = state.clone();
            match tokio::task::spawn_blocking(move || s.reload_if_changed()).await {
                Ok(Ok(_)) => {}
                // Keep serving the old data; the next tick retries.
                Ok(Err(e)) => error!(error = %e, "reload failed; still serving previous data"),
                Err(e) => error!(error = %e, "reload task panicked"),
            }
        }
    })
}
