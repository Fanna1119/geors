//! Shared types for geors: layers, place models, geometry helpers and the
//! on-disk partition format.
//!
//! This crate has no heavy dependencies so that every other crate (ingestion,
//! indexing, ranking, HTTP) can agree on the same vocabulary.

pub mod geom;
pub mod layer;
pub mod model;
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
