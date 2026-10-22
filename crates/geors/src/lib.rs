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
//! Work in progress (issue #42): importing and updating move here next.

mod error;
mod geocoder;
#[cfg(feature = "geojson")]
pub mod geojson;
pub mod request;

pub use error::{Error, Result};
pub use geocoder::{Geocoder, GeocoderConfig, PartitionInfo, Status};
pub use request::{Contains, Nearest, Reverse, Search};

pub use geors_core::{AdminUnit, BBox, Layer, LonLat, OsmType, Place};
pub use geors_index::synonyms::Synonyms;
pub use geors_index::{Address, Hit, PlaceRef, Shape, TagFilter};
pub use geors_rank::RankingConfig;
