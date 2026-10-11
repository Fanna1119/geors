//! Prometheus metrics (`GET /metrics`, text exposition format 0.0.4).
//!
//! Per endpoint: request and error counts and a latency histogram,
//! collected by a middleware with plain atomics (no locks on the request
//! path). Plus the loaded data: partitions, places per country, data
//! generation and load time.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::state::AppState;

/// Endpoints that are counted separately; everything else is "other".
const ENDPOINTS: [&str; 7] = [
    "search", "reverse", "nearest", "lookup", "contains", "status", "other",
];

/// Histogram bucket upper bounds, seconds.
const BUCKETS: [f64; 11] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
];

#[derive(Default)]
struct Endpoint {
    requests: AtomicU64,
    errors: AtomicU64,
    micros: AtomicU64,
    /// Requests per bucket (not cumulative); the last one is +Inf.
    buckets: [AtomicU64; BUCKETS.len() + 1],
}

#[derive(Default)]
pub struct Metrics {
    endpoints: [Endpoint; ENDPOINTS.len()],
}

/// The endpoint a request path is counted under.
fn endpoint(path: &str) -> usize {
    let name = match path {
        "/search" | "/api" => "search",
        "/reverse" => "reverse",
        "/nearest" => "nearest",
        "/lookup" => "lookup",
        "/contains" => "contains",
        "/status" => "status",
        _ => "other",
    };
    ENDPOINTS.iter().position(|e| *e == name).unwrap()
}

impl Metrics {
    pub fn record(&self, path: &str, status: u16, elapsed: Duration) {
        let e = &self.endpoints[endpoint(path)];
        e.requests.fetch_add(1, Ordering::Relaxed);
        if status >= 400 {
            e.errors.fetch_add(1, Ordering::Relaxed);
        }
        e.micros
            .fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
        let secs = elapsed.as_secs_f64();
        let b = BUCKETS
            .iter()
            .position(|&le| secs <= le)
            .unwrap_or(BUCKETS.len());
        e.buckets[b].fetch_add(1, Ordering::Relaxed);
    }

    /// The metrics page.
    pub fn render(&self, state: &AppState) -> String {
        let mut out = String::new();
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let header = |out: &mut String, name: &str, kind: &str, help: &str| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        };
        header(
            &mut out,
            "geors_requests_total",
            "counter",
            "HTTP requests by endpoint.",
        );
        for (name, e) in ENDPOINTS.iter().zip(&self.endpoints) {
            let _ = writeln!(
                out,
                "geors_requests_total{{endpoint=\"{name}\"}} {}",
                load(&e.requests)
            );
        }
        header(
            &mut out,
            "geors_request_errors_total",
            "counter",
            "HTTP responses with status 4xx or 5xx by endpoint.",
        );
        for (name, e) in ENDPOINTS.iter().zip(&self.endpoints) {
            let _ = writeln!(
                out,
                "geors_request_errors_total{{endpoint=\"{name}\"}} {}",
                load(&e.errors)
            );
        }
        header(
            &mut out,
            "geors_request_duration_seconds",
            "histogram",
            "Request latency by endpoint.",
        );
        for (name, e) in ENDPOINTS.iter().zip(&self.endpoints) {
            let mut cumulative = 0;
            for (i, le) in BUCKETS
                .iter()
                .map(|b| b.to_string())
                .chain(["+Inf".into()])
                .enumerate()
            {
                cumulative += load(&e.buckets[i]);
                let _ = writeln!(
                    out,
                    "geors_request_duration_seconds_bucket{{endpoint=\"{name}\",le=\"{le}\"}} {cumulative}"
                );
            }
            let _ = writeln!(
                out,
                "geors_request_duration_seconds_sum{{endpoint=\"{name}\"}} {}",
                load(&e.micros) as f64 / 1e6
            );
            let _ = writeln!(
                out,
                "geors_request_duration_seconds_count{{endpoint=\"{name}\"}} {}",
                load(&e.requests)
            );
        }

        let engine = state.engine();
        header(
            &mut out,
            "geors_partitions",
            "gauge",
            "Loaded country partitions.",
        );
        let _ = writeln!(out, "geors_partitions {}", engine.partitions().len());
        header(
            &mut out,
            "geors_places",
            "gauge",
            "Places per loaded partition.",
        );
        for p in engine.partitions() {
            let _ = writeln!(
                out,
                "geors_places{{country=\"{}\"}} {}",
                p.meta.country_code, p.meta.num_places
            );
        }
        let info = state.load_info();
        header(
            &mut out,
            "geors_data_loaded_timestamp_seconds",
            "gauge",
            "When the loaded data was opened (Unix time).",
        );
        let _ = writeln!(
            out,
            "geors_data_loaded_timestamp_seconds {}",
            info.loaded_unix
        );
        header(
            &mut out,
            "geors_build_info",
            "gauge",
            "Version and loaded data generation (value is always 1).",
        );
        let _ = writeln!(
            out,
            "geors_build_info{{version=\"{}\",generation=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION"),
            info.generation
                .unwrap_or_default()
                .replace(['"', '\\', '\n'], "")
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_by_endpoint_and_bucket() {
        let m = Metrics::default();
        m.record("/api", 200, Duration::from_millis(3));
        m.record("/search", 400, Duration::from_millis(30));
        m.record("/nope", 404, Duration::from_secs(10));
        let s = &m.endpoints[endpoint("/search")];
        assert_eq!(s.requests.load(Ordering::Relaxed), 2);
        assert_eq!(s.errors.load(Ordering::Relaxed), 1);
        // 3 ms in the <= 5 ms bucket, 30 ms in the <= 50 ms bucket.
        assert_eq!(s.buckets[2].load(Ordering::Relaxed), 1);
        assert_eq!(s.buckets[5].load(Ordering::Relaxed), 1);
        let o = &m.endpoints[endpoint("/nope")];
        assert_eq!(o.buckets[BUCKETS.len()].load(Ordering::Relaxed), 1);
    }
}
