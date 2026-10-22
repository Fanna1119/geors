//! Requests, built with defaults and checked when they run.
//!
//! ```
//! use geors::{LonLat, Layer, Search};
//!
//! let wien = LonLat::new(16.37, 48.21);
//! let search = Search::text("Stephansplatz")
//!     .near(wien)
//!     .layers([Layer::Street, Layer::Poi])
//!     .limit(5);
//! ```

use geors_core::{BBox, Layer, LonLat, normalize_country_code};
use geors_index::{NearestRequest, SearchRequest, TagFilter};

use crate::{Error, Result};

/// Layers `Reverse` returns unless told otherwise: what is "at" a point.
pub const REVERSE_LAYERS: [Layer; 4] = [Layer::House, Layer::Poi, Layer::Street, Layer::Locality];
/// Radius `Reverse` searches unless told otherwise, metres.
pub const REVERSE_RADIUS_M: f64 = 1_000.0;

/// Text search, optionally filtered and biased by location.
///
/// Without text, returns the places inside the spatial filter: nearest
/// first with a point, most important first with only a box.
#[derive(Debug, Clone)]
pub struct Search {
    query: Option<String>,
    focus: Option<LonLat>,
    radius_m: Option<f64>,
    bbox: Option<BBox>,
    countries: Vec<String>,
    layers: Vec<Layer>,
    tags: TagFilter,
    limit: usize,
    autocomplete: bool,
}

impl Search {
    /// Search for `query` (search-as-you-type: the last word may be a
    /// prefix, see [`Search::autocomplete`]).
    pub fn text(query: impl Into<String>) -> Self {
        Self {
            query: Some(query.into()),
            ..Self::empty()
        }
    }

    /// No text: places around `point`, nearest first (use with
    /// [`Search::radius_m`] and [`Search::layers`]).
    pub fn around(point: LonLat) -> Self {
        Self {
            focus: Some(point),
            ..Self::empty()
        }
    }

    /// No text: places inside `bbox`, most important first.
    pub fn within(bbox: BBox) -> Self {
        Self {
            bbox: Some(bbox),
            ..Self::empty()
        }
    }

    fn empty() -> Self {
        Self {
            query: None,
            focus: None,
            radius_m: None,
            bbox: None,
            countries: Vec::new(),
            layers: Vec::new(),
            tags: TagFilter::default(),
            limit: 10,
            autocomplete: true,
        }
    }

    /// Rank places near `point` higher (and the centre for
    /// [`Search::radius_m`]).
    pub fn near(mut self, point: LonLat) -> Self {
        self.focus = Some(point);
        self
    }

    /// Only places within this distance of the point (needs
    /// [`Search::near`] or [`Search::around`]).
    pub fn radius_m(mut self, metres: f64) -> Self {
        self.radius_m = Some(metres);
        self
    }

    /// Only places intersecting `bbox`.
    pub fn bbox(mut self, bbox: BBox) -> Self {
        self.bbox = Some(bbox);
        self
    }

    /// Only these countries (ISO 3166-1 alpha-2 codes, any case).
    pub fn countries<S: AsRef<str>>(mut self, codes: impl IntoIterator<Item = S>) -> Self {
        self.countries = codes.into_iter().map(|c| c.as_ref().to_string()).collect();
        self
    }

    /// Only these layers (empty: all).
    pub fn layers(mut self, layers: impl IntoIterator<Item = Layer>) -> Self {
        self.layers = layers.into_iter().collect();
        self
    }

    /// Only places matching these OSM tags (see [`TagFilter`]).
    pub fn tags(mut self, tags: TagFilter) -> Self {
        self.tags = tags;
        self
    }

    /// At most this many results (default 10).
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Treat the last word as a prefix unless the text ends with a space
    /// (default `true`).
    pub fn autocomplete(mut self, on: bool) -> Self {
        self.autocomplete = on;
        self
    }

    pub(crate) fn build(&self) -> Result<SearchRequest> {
        let focus = self.focus.map(check_point).transpose()?;
        let radius_m = self.radius_m.map(check_radius).transpose()?;
        if radius_m.is_some() && focus.is_none() {
            return Err(invalid("a radius needs a point (near / around)"));
        }
        let query = self.query.clone().filter(|q| !q.trim().is_empty());
        if query.is_none() && focus.is_none() && self.bbox.is_none() {
            return Err(invalid("provide a text, a point or a bounding box"));
        }
        Ok(SearchRequest {
            query,
            focus,
            radius_m,
            bbox: self.bbox,
            countries: countries(&self.countries)?,
            layers: self.layers.clone(),
            limit: check_limit(self.limit)?,
            autocomplete: self.autocomplete,
            tags: self.tags.clone(),
        })
    }
}

/// Reverse geocoding: what is at a point. By default one result within
/// [`REVERSE_RADIUS_M`] among [`REVERSE_LAYERS`]; distances to streets are
/// measured to their line.
#[derive(Debug, Clone)]
pub struct Reverse {
    point: LonLat,
    radius_m: f64,
    layers: Vec<Layer>,
    countries: Vec<String>,
    tags: TagFilter,
    limit: usize,
}

impl Reverse {
    pub fn at(point: LonLat) -> Self {
        Self {
            point,
            radius_m: REVERSE_RADIUS_M,
            layers: REVERSE_LAYERS.to_vec(),
            countries: Vec::new(),
            tags: TagFilter::default(),
            limit: 1,
        }
    }

    pub fn radius_m(mut self, metres: f64) -> Self {
        self.radius_m = metres;
        self
    }

    /// Layers to consider (empty: [`REVERSE_LAYERS`]).
    pub fn layers(mut self, layers: impl IntoIterator<Item = Layer>) -> Self {
        self.layers = layers.into_iter().collect();
        if self.layers.is_empty() {
            self.layers = REVERSE_LAYERS.to_vec();
        }
        self
    }

    pub fn countries<S: AsRef<str>>(mut self, codes: impl IntoIterator<Item = S>) -> Self {
        self.countries = codes.into_iter().map(|c| c.as_ref().to_string()).collect();
        self
    }

    pub fn tags(mut self, tags: TagFilter) -> Self {
        self.tags = tags;
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub(crate) fn build(&self) -> Result<NearestRequest> {
        Ok(NearestRequest {
            point: check_point(self.point)?,
            max_distance_m: Some(check_radius(self.radius_m)?),
            countries: countries(&self.countries)?,
            layers: self.layers.clone(),
            limit: check_limit(self.limit)?,
            tags: self.tags.clone(),
        })
    }
}

/// The exact nearest places to a point, at any distance unless capped.
/// By default 5, of all layers.
#[derive(Debug, Clone)]
pub struct Nearest {
    point: LonLat,
    max_distance_m: Option<f64>,
    layers: Vec<Layer>,
    countries: Vec<String>,
    tags: TagFilter,
    limit: usize,
}

impl Nearest {
    pub fn to(point: LonLat) -> Self {
        Self {
            point,
            max_distance_m: None,
            layers: Vec::new(),
            countries: Vec::new(),
            tags: TagFilter::default(),
            limit: 5,
        }
    }

    pub fn max_distance_m(mut self, metres: f64) -> Self {
        self.max_distance_m = Some(metres);
        self
    }

    pub fn layers(mut self, layers: impl IntoIterator<Item = Layer>) -> Self {
        self.layers = layers.into_iter().collect();
        self
    }

    pub fn countries<S: AsRef<str>>(mut self, codes: impl IntoIterator<Item = S>) -> Self {
        self.countries = codes.into_iter().map(|c| c.as_ref().to_string()).collect();
        self
    }

    pub fn tags(mut self, tags: TagFilter) -> Self {
        self.tags = tags;
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub(crate) fn build(&self) -> Result<NearestRequest> {
        Ok(NearestRequest {
            point: check_point(self.point)?,
            max_distance_m: self.max_distance_m.map(check_radius).transpose()?,
            countries: countries(&self.countries)?,
            layers: self.layers.clone(),
            limit: check_limit(self.limit)?,
            tags: self.tags.clone(),
        })
    }
}

/// Areas whose polygon contains a point, most specific first
/// (district < city < county < state < country).
#[derive(Debug, Clone)]
pub struct Contains {
    point: LonLat,
    layers: Vec<Layer>,
    countries: Vec<String>,
}

impl Contains {
    pub fn at(point: LonLat) -> Self {
        Self {
            point,
            layers: Vec::new(),
            countries: Vec::new(),
        }
    }

    /// Only these area layers (locality and up; empty: all).
    pub fn layers(mut self, layers: impl IntoIterator<Item = Layer>) -> Self {
        self.layers = layers.into_iter().collect();
        self
    }

    pub fn countries<S: AsRef<str>>(mut self, codes: impl IntoIterator<Item = S>) -> Self {
        self.countries = codes.into_iter().map(|c| c.as_ref().to_string()).collect();
        self
    }

    pub(crate) fn build(&self) -> Result<(LonLat, Vec<String>, Vec<Layer>)> {
        if let Some(l) = self.layers.iter().find(|l| **l < Layer::Locality) {
            return Err(invalid(format!(
                "'{}' is not an area layer (locality, district, city, county, state, country)",
                l.as_str()
            )));
        }
        Ok((
            check_point(self.point)?,
            countries(&self.countries)?,
            self.layers.clone(),
        ))
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidRequest(msg.into())
}

fn check_point(p: LonLat) -> Result<LonLat> {
    if p.is_valid() {
        Ok(p)
    } else {
        Err(invalid(format!(
            "coordinates out of range: lat={} must be in [-90, 90], lon={} in [-180, 180]",
            p.lat, p.lon
        )))
    }
}

fn check_radius(r: f64) -> Result<f64> {
    if r.is_finite() && r > 0.0 {
        Ok(r)
    } else {
        Err(invalid("a radius must be a positive number of metres"))
    }
}

fn check_limit(limit: usize) -> Result<usize> {
    if limit == 0 {
        Err(invalid("the limit must be at least 1"))
    } else {
        Ok(limit)
    }
}

/// Lowercase ISO codes; an error for anything that is not one.
fn countries(codes: &[String]) -> Result<Vec<String>> {
    codes
        .iter()
        .map(|c| {
            normalize_country_code(c).ok_or_else(|| {
                invalid(format!(
                    "invalid country code '{c}' (expected ISO 3166-1 alpha-2, e.g. DE)"
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        let p = LonLat::new(9.5, 47.1);
        assert!(Search::text("vaduz").build().is_ok());
        assert!(Search::text("  ").build().is_err());
        assert!(Search::text("x").radius_m(100.0).build().is_err());
        assert!(Search::text("x").near(p).radius_m(-1.0).build().is_err());
        assert!(
            Search::text("x")
                .near(LonLat::new(9.5, 99.0))
                .build()
                .is_err()
        );
        assert!(Search::text("x").limit(0).build().is_err());
        assert!(Search::text("x").countries(["DEU"]).build().is_err());
        let r = Search::around(p)
            .radius_m(500.0)
            .countries(["li", "AT"])
            .build()
            .unwrap();
        assert_eq!(r.countries, vec!["li", "at"]);
        assert_eq!(r.radius_m, Some(500.0));
        // Reverse defaults; an empty layer list means the defaults.
        let r = Reverse::at(p).layers([]).build().unwrap();
        assert_eq!(
            (r.limit, r.max_distance_m, r.layers.len()),
            (1, Some(1_000.0), 4)
        );
        assert_eq!(Nearest::to(p).build().unwrap().limit, 5);
        assert!(Contains::at(p).layers([Layer::House]).build().is_err());
        assert!(Contains::at(p).layers([Layer::City]).build().is_ok());
    }
}
