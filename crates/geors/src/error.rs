use geors_index::{EngineError, IndexError};

/// Everything that can go wrong opening data or answering a request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The request is invalid (a radius without a point, coordinates out
    /// of range, an unknown layer, ...). Safe to show to the caller.
    #[error("{0}")]
    InvalidRequest(String),
    /// A requested country has no partition in the data directory.
    #[error("country '{requested}' is not loaded (loaded: {})", loaded.join(", "))]
    CountryNotLoaded {
        requested: String,
        loaded: Vec<String>,
    },
    /// The data directory or a partition could not be read.
    #[error(transparent)]
    Data(#[from] IndexError),
}

impl From<EngineError> for Error {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::BadRequest(m) => Error::InvalidRequest(m),
            EngineError::CountryNotLoaded { requested, loaded } => {
                Error::CountryNotLoaded { requested, loaded }
            }
            EngineError::Index(e) => Error::Data(e),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
