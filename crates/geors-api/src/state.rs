//! Shared server state: the library's [`Geocoder`] plus HTTP settings.
//!
//! Imports and updates replace partition directories atomically and bump
//! `<data>/GENERATION`; [`spawn_watcher`] polls it and lets the geocoder
//! swap in the new data (requests in flight finish on the old data).

use std::sync::Arc;
use std::time::Duration;

use geors::Geocoder;
use tracing::error;

use crate::ApiConfig;

pub struct AppState {
    pub geocoder: Geocoder,
    pub config: ApiConfig,
    pub metrics: crate::metrics::Metrics,
}

impl AppState {
    pub fn new(geocoder: Geocoder, config: ApiConfig) -> Self {
        Self {
            geocoder,
            config,
            metrics: Default::default(),
        }
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
            match tokio::task::spawn_blocking(move || s.geocoder.reload_if_changed()).await {
                Ok(Ok(_)) => {}
                // Keep serving the old data; the next tick retries.
                Ok(Err(e)) => error!(error = %e, "reload failed; still serving previous data"),
                Err(e) => error!(error = %e, "reload task panicked"),
            }
        }
    })
}
