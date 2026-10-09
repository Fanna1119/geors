//! Spatial (packed Hilbert R-tree) and text (tantivy) indexing for geors,
//! organised as one self-contained partition per country.

pub mod bitset;
pub mod engine;
pub mod partition;
pub mod synonyms;
pub mod text;
pub mod writer;

pub use engine::{Address, Engine, EngineConfig, Hit, NearestRequest, SearchRequest, Shape};
pub use partition::Partition;
pub use writer::{PartitionInput, write_partition};

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("text index error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Format(String),
}

/// Errors surfaced to API callers.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    BadRequest(String),
    #[error("country '{requested}' is not loaded (loaded: {})", loaded.join(", "))]
    CountryNotLoaded {
        requested: String,
        loaded: Vec<String>,
    },
    #[error(transparent)]
    Index(#[from] IndexError),
}
