//! Shared types for geors: layers, place models, geometry helpers and the
//! on-disk partition format.
//!
//! This crate has no heavy dependencies so that every other crate (ingestion,
//! indexing, ranking, HTTP) can agree on the same vocabulary.

pub mod doc;
pub mod extsort;
pub mod geom;
pub mod layer;
pub mod mem;
pub mod model;
pub mod spill;
pub mod storage;

pub use geom::{BBox, LonLat};
pub use layer::Layer;
pub use model::{AdminUnit, OsmType, Place};

/// Normalise a user supplied ISO 3166-1 alpha-2 code ("de", " DE ") to the
/// canonical lowercase form used for partition directory names.
pub fn normalize_country_code(code: &str) -> Option<String> {
    let code = code.trim();
    if code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        Some(code.to_ascii_lowercase())
    } else {
        None
    }
}

/// Memory available to this process: the container limit if there is one,
/// else physical RAM. `None` if unknown.
pub fn memory_limit() -> Option<u64> {
    let read = |p: &str| std::fs::read_to_string(p).ok();
    // cgroup v2, then v1 (v1 reports a huge number when unlimited).
    if let Some(v) = read("/sys/fs/cgroup/memory.max").and_then(|s| s.trim().parse::<u64>().ok()) {
        return Some(v);
    }
    if let Some(v) = read("/sys/fs/cgroup/memory/memory.limit_in_bytes")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| v < 1 << 60)
    {
        return Some(v);
    }
    if let Some(kb) = read("/proc/meminfo").and_then(|m| {
        m.lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
    }) {
        return Some(kb * 1024);
    }
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_codes() {
        assert_eq!(normalize_country_code(" DE "), Some("de".into()));
        assert_eq!(normalize_country_code("deu"), None);
        assert_eq!(normalize_country_code("d1"), None);
    }
}
