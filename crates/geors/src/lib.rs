//! geors: a fast, lightweight geocoder for OpenStreetMap data.
//!
//! Open the per-country indexes built by `geors import` and run text
//! search, reverse geocoding, nearest places, lookups by OSM id and area
//! containment, without the HTTP server.
//!
//! ```no_run
//! use geors::{Geocoder, GeocoderConfig, LonLat, Reverse, Search};
//!
//! let geocoder = Geocoder::open("data", GeocoderConfig::default())?;
//! for hit in geocoder.search(&Search::text("Stephansplatz 1, Wien").limit(3))? {
//!     let address = hit.address(Some("de"));
//!     println!("{:?} in {:?}", hit.place.name, address.city);
//! }
//! let here = geocoder.reverse(&Reverse::at(LonLat::new(16.3731, 48.2085)))?;
//! # Ok::<(), geors::Error>(())
//! ```
//!
//! # Sharing a geocoder
//!
//! A [`Geocoder`] is `Send + Sync`: open it once and share it (`Arc`, or a
//! web framework's state). Queries are blocking, CPU-bound calls that read
//! memory-mapped files; in async code run them with
//! `tokio::task::spawn_blocking` or similar. Call
//! [`Geocoder::reload_if_changed`] (on a timer, or after an import) to
//! switch to new data without interrupting queries in flight.
//!
//! ```no_run
//! use std::sync::Arc;
//! use geors::{Geocoder, GeocoderConfig, Search};
//!
//! let geocoder = Arc::new(Geocoder::open("data", GeocoderConfig::default())?);
//! let workers: Vec<_> = ["Vaduz", "Schaan"]
//!     .into_iter()
//!     .map(|q| {
//!         let geocoder = geocoder.clone();
//!         std::thread::spawn(move || geocoder.search(&Search::text(q).limit(1)))
//!     })
//!     .collect();
//! for w in workers {
//!     println!("{:?}", w.join().unwrap()?.first().map(|h| h.place.center));
//! }
//! # Ok::<(), geors::Error>(())
//! ```
//!
//! # Features
//!
//! - `geojson`: results as GeoJSON features ([`geojson`] module).
//! - `import`: import local `.osm.pbf` files and Nominatim/Photon dumps
//!   ([`import`] module). No network code.
//! - `update`: registered sources, downloads (Geofabrik, URLs) and
//!   replication-diff updates ([`update`] module); implies `import`.
//!
//! The `geors-*` crates this one is built on are internal: their APIs
//! change between releases without notice.

pub mod data;
mod error;
mod geocoder;
#[cfg(feature = "geojson")]
pub mod geojson;
#[cfg(feature = "import")]
pub mod import;
pub mod request;
pub mod synonyms;
#[cfg(feature = "update")]
pub mod update;

pub use error::{Error, Result};
pub use geocoder::{Geocoder, GeocoderConfig, PartitionInfo, Status};
pub use request::{Contains, Nearest, Reverse, Search};

pub use geors_core::{AdminUnit, BBox, Layer, LonLat, OsmType, Place, normalize_country_code};
pub use geors_index::{Address, Hit, PlaceRef, Shape, TagFilter};
pub use geors_rank::RankingConfig;
pub use synonyms::Synonyms;

/// Allocator that counts heap use, for the `mem-stats` import diagnostics.
#[doc(hidden)]
pub use geors_core::mem;

#[cfg(test)]
mod tests {
    #[test]
    fn geocoder_is_shareable() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<crate::Geocoder>();
    }
}
