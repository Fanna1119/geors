//! HTTP API: `/search`, `/reverse`, `/nearest`, `/lookup`, `/status`.
//!
//! All responses are GeoJSON FeatureCollections (except `/status`); errors
//! are `{"error": "..."}` with a 4xx/5xx status.

mod error;
mod params;
mod render;
mod state;

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::response::Json;
use axum::routing::get;
use geors_index::NearestRequest;
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::debug;

pub use error::ApiError;
use params::{LookupParams, PointParams, SearchParams};
pub use state::{AppState, DataConfig, LoadInfo, spawn_watcher};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub default_limit: usize,
    pub max_limit: usize,
    /// Default search radius for `/reverse`, metres.
    pub reverse_radius_m: f64,
    /// Upper bound for any `radius` parameter, metres.
    pub max_radius_m: f64,
    /// Add permissive CORS headers.
    pub cors: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            default_limit: 10,
            max_limit: 50,
            reverse_radius_m: 1_000.0,
            max_radius_m: 500_000.0,
            cors: true,
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let cors = state.config.cors;
    let router = Router::new()
        .route("/search", get(search))
        .route("/api", get(search)) // Photon-compatible alias
        .route("/reverse", get(reverse))
        .route("/nearest", get(nearest))
        .route("/lookup", get(lookup))
        .route("/status", get(status))
        .with_state(state)
        .layer(TraceLayer::new_for_http());
    if cors {
        router.layer(CorsLayer::permissive())
    } else {
        router
    }
}

/// Engine calls are blocking (mmap + tantivy); keep them off the async workers.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("worker failed: {e}")))?
}

type GeoJson = Result<Json<geojson::FeatureCollection>, ApiError>;

async fn search(
    State(state): State<Arc<AppState>>,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> GeoJson {
    let Query(params) = params?;
    let out = params.output()?;
    let req = params.into_request(&state.config)?;
    let t = Instant::now();
    let fc = blocking(move || {
        let hits = state.engine().search(&req)?;
        Ok(render::collection(&hits, &out))
    })
    .await?;
    debug!(elapsed = ?t.elapsed(), results = fc.features.len(), "search");
    Ok(Json(fc))
}

async fn reverse(
    State(state): State<Arc<AppState>>,
    params: Result<Query<PointParams>, QueryRejection>,
) -> GeoJson {
    let Query(params) = params?;
    let out = params.output()?;
    let req = params.into_reverse(&state.config)?;
    run_nearest(state, req, out).await
}

async fn nearest(
    State(state): State<Arc<AppState>>,
    params: Result<Query<PointParams>, QueryRejection>,
) -> GeoJson {
    let Query(params) = params?;
    let out = params.output()?;
    let req = params.into_nearest(&state.config)?;
    run_nearest(state, req, out).await
}

async fn run_nearest(state: Arc<AppState>, req: NearestRequest, out: render::Output) -> GeoJson {
    blocking(move || {
        let hits = state.engine().nearest(&req)?;
        Ok(Json(render::collection(&hits, &out)))
    })
    .await
}

async fn lookup(
    State(state): State<Arc<AppState>>,
    params: Result<Query<LookupParams>, QueryRejection>,
) -> GeoJson {
    let Query(params) = params?;
    let out = params.output()?;
    let (ids, countries) = params.into_ids(&state.config)?;
    blocking(move || {
        let hits = state.engine().lookup(&ids, &countries)?;
        Ok(Json(render::collection(&hits, &out)))
    })
    .await
}

#[derive(Serialize)]
struct PartitionStatus {
    country_code: String,
    country_name: Option<String>,
    places: u32,
    layers: std::collections::BTreeMap<String, u32>,
    bbox: [f64; 4],
    source: String,
    created_unix: u64,
}

#[derive(Serialize)]
struct Status {
    status: &'static str,
    version: &'static str,
    /// Value of `<data>/GENERATION` when the data was loaded.
    generation: Option<String>,
    loaded_unix: u64,
    partitions: Vec<PartitionStatus>,
    /// Contents of `<data>/sources.json` (import origin and update state).
    #[serde(skip_serializing_if = "Option::is_none")]
    sources: Option<serde_json::Value>,
}

async fn status(State(state): State<Arc<AppState>>) -> Json<Status> {
    let engine = state.engine();
    let partitions = engine
        .partitions()
        .iter()
        .map(|p| {
            let m = &p.meta;
            PartitionStatus {
                country_code: m.country_code.clone(),
                country_name: m.country_name.clone(),
                places: m.num_places,
                layers: m.layers.clone(),
                bbox: [
                    m.bbox.min_lon,
                    m.bbox.min_lat,
                    m.bbox.max_lon,
                    m.bbox.max_lat,
                ],
                source: m.source.clone(),
                created_unix: m.created_unix,
            }
        })
        .collect();
    let sources = std::fs::read(state.data.data_dir.join("sources.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|mut v| v.get_mut("sources").map(serde_json::Value::take));
    let info = state.load_info();
    Json(Status {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        generation: info.generation,
        loaded_unix: info.loaded_unix,
        partitions,
        sources,
    })
}

/// Serve the API on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}
