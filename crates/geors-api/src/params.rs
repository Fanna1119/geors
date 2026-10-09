//! Query-string parameters and their validation.

use geors_core::geom::{BBox, LonLat};
use geors_core::{Layer, OsmType, normalize_country_code};
use geors_index::{NearestRequest, SearchRequest};
use serde::Deserialize;

use crate::render::Output;
use crate::{ApiConfig, ApiError};

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub radius: Option<f64>,
    pub bbox: Option<String>,
    pub country: Option<String>,
    pub limit: Option<usize>,
    pub layers: Option<String>,
    pub lang: Option<String>,
    pub autocomplete: Option<bool>,
    pub geometry: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PointParams {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub radius: Option<f64>,
    pub limit: Option<usize>,
    pub layers: Option<String>,
    pub country: Option<String>,
    pub lang: Option<String>,
    pub geometry: Option<String>,
}

pub type OsmIds = Vec<(OsmType, i64)>;

#[derive(Debug, Deserialize)]
pub struct LookupParams {
    pub osm_ids: Option<String>,
    pub country: Option<String>,
    pub lang: Option<String>,
    pub geometry: Option<String>,
}

fn output(lang: &Option<String>, geometry: &Option<String>) -> Result<Output, ApiError> {
    let full_geometry = match geometry.as_deref().map(str::trim) {
        None | Some("") | Some("point") => false,
        Some("full") => true,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "invalid geometry '{other}' (expected 'point' or 'full')"
            )));
        }
    };
    Ok(Output {
        lang: lang.clone(),
        full_geometry,
    })
}

impl LookupParams {
    pub fn output(&self) -> Result<Output, ApiError> {
        output(&self.lang, &self.geometry)
    }

    /// Parse `osm_ids=N123,W456,R789`.
    /// Returns the requested ids and country filter.
    pub fn into_ids(self, cfg: &ApiConfig) -> Result<(OsmIds, Vec<String>), ApiError> {
        let ids: Vec<(OsmType, i64)> = comma_list(&self.osm_ids)
            .map(|s| {
                let (t, n) = s.split_at(1);
                let t = match t.to_ascii_uppercase().as_str() {
                    "N" => OsmType::Node,
                    "W" => OsmType::Way,
                    "R" => OsmType::Relation,
                    _ => {
                        return Err(ApiError::bad_request(format!(
                            "invalid OSM id '{s}' (expected e.g. N123, W456, R789)"
                        )));
                    }
                };
                n.parse::<i64>().map(|n| (t, n)).map_err(|_| {
                    ApiError::bad_request(format!(
                        "invalid OSM id '{s}' (expected e.g. N123, W456, R789)"
                    ))
                })
            })
            .collect::<Result<_, _>>()?;
        if ids.is_empty() {
            return Err(ApiError::bad_request(
                "'osm_ids' is required, e.g. osm_ids=N123,W456",
            ));
        }
        if ids.len() > cfg.max_limit {
            return Err(ApiError::bad_request(format!(
                "at most {} ids per request",
                cfg.max_limit
            )));
        }
        Ok((ids, countries(&self.country)?))
    }
}

fn point(lat: Option<f64>, lon: Option<f64>) -> Result<Option<LonLat>, ApiError> {
    match (lat, lon) {
        (None, None) => Ok(None),
        (Some(lat), Some(lon)) => {
            let p = LonLat::new(lon, lat);
            if p.is_valid() {
                Ok(Some(p))
            } else {
                Err(ApiError::bad_request(format!(
                    "coordinates out of range: lat={lat} must be in [-90, 90], lon={lon} in [-180, 180]"
                )))
            }
        }
        _ => Err(ApiError::bad_request(
            "'lat' and 'lon' must be given together",
        )),
    }
}

fn required_point(lat: Option<f64>, lon: Option<f64>) -> Result<LonLat, ApiError> {
    point(lat, lon)?.ok_or_else(|| ApiError::bad_request("'lat' and 'lon' are required"))
}

fn limit(v: Option<usize>, default: usize, cfg: &ApiConfig) -> Result<usize, ApiError> {
    match v.unwrap_or(default) {
        0 => Err(ApiError::bad_request("'limit' must be at least 1")),
        n if n > cfg.max_limit => Err(ApiError::bad_request(format!(
            "'limit' must be at most {}",
            cfg.max_limit
        ))),
        n => Ok(n),
    }
}

fn radius(v: Option<f64>, cfg: &ApiConfig) -> Result<Option<f64>, ApiError> {
    match v {
        None => Ok(None),
        Some(r) if !(r.is_finite() && r > 0.0) => Err(ApiError::bad_request(
            "'radius' must be a positive number of metres",
        )),
        Some(r) if r > cfg.max_radius_m => Err(ApiError::bad_request(format!(
            "'radius' must be at most {} metres",
            cfg.max_radius_m
        ))),
        Some(r) => Ok(Some(r)),
    }
}

fn comma_list(v: &Option<String>) -> impl Iterator<Item = &str> {
    v.as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn layers(v: &Option<String>) -> Result<Vec<Layer>, ApiError> {
    comma_list(v)
        .map(|s| s.parse::<Layer>().map_err(ApiError::bad_request))
        .collect()
}

fn countries(v: &Option<String>) -> Result<Vec<String>, ApiError> {
    comma_list(v)
        .map(|s| {
            normalize_country_code(s).ok_or_else(|| {
                ApiError::bad_request(format!(
                    "invalid country code '{s}' (expected ISO 3166-1 alpha-2, e.g. DE)"
                ))
            })
        })
        .collect()
}

impl SearchParams {
    pub fn output(&self) -> Result<Output, ApiError> {
        output(&self.lang, &self.geometry)
    }

    pub fn into_request(self, cfg: &ApiConfig) -> Result<SearchRequest, ApiError> {
        let focus = point(self.lat, self.lon)?;
        let radius = radius(self.radius, cfg)?;
        if radius.is_some() && focus.is_none() {
            return Err(ApiError::bad_request("'radius' requires 'lat' and 'lon'"));
        }
        let bbox = self
            .bbox
            .as_deref()
            .map(BBox::parse)
            .transpose()
            .map_err(ApiError::bad_request)?;
        let query = self.q.filter(|q| !q.trim().is_empty());
        if query.is_none() && focus.is_none() && bbox.is_none() {
            return Err(ApiError::bad_request(
                "provide a query 'q', a location ('lat'/'lon'), or a 'bbox'",
            ));
        }
        Ok(SearchRequest {
            query,
            focus,
            radius_m: radius,
            bbox,
            countries: countries(&self.country)?,
            layers: layers(&self.layers)?,
            limit: limit(self.limit, cfg.default_limit, cfg)?,
            autocomplete: self.autocomplete.unwrap_or(true),
        })
    }
}

impl PointParams {
    pub fn output(&self) -> Result<Output, ApiError> {
        output(&self.lang, &self.geometry)
    }

    /// `/reverse`: what is at this point? Defaults to one result within
    /// `reverse_radius_m`, restricted to address-like layers.
    pub fn into_reverse(self, cfg: &ApiConfig) -> Result<NearestRequest, ApiError> {
        let mut layers = layers(&self.layers)?;
        if layers.is_empty() {
            layers = vec![Layer::House, Layer::Poi, Layer::Street, Layer::Locality];
        }
        Ok(NearestRequest {
            point: required_point(self.lat, self.lon)?,
            max_distance_m: Some(radius(self.radius, cfg)?.unwrap_or(cfg.reverse_radius_m)),
            countries: countries(&self.country)?,
            layers,
            limit: limit(self.limit, 1, cfg)?,
        })
    }

    /// `/nearest`: no default radius or layer filter.
    pub fn into_nearest(self, cfg: &ApiConfig) -> Result<NearestRequest, ApiError> {
        Ok(NearestRequest {
            point: required_point(self.lat, self.lon)?,
            max_distance_m: radius(self.radius, cfg)?,
            countries: countries(&self.country)?,
            layers: layers(&self.layers)?,
            limit: limit(self.limit, 5, cfg)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(q: &str) -> Result<SearchRequest, ApiError> {
        let p: SearchParams = serde_urlencoded::from_str(q).unwrap();
        p.into_request(&ApiConfig::default())
    }

    #[test]
    fn validation() {
        assert!(search("q=vaduz").is_ok());
        assert!(search("").is_err());
        assert!(search("q=x&lat=47").is_err());
        assert!(search("q=x&radius=100").is_err());
        assert!(search("q=x&lat=47&lon=9.5&radius=-1").is_err());
        assert!(search("q=x&lat=99&lon=9.5").is_err());
        assert!(search("q=x&limit=0").is_err());
        assert!(search("q=x&limit=1000").is_err());
        assert!(search("q=x&layers=city,planet").is_err());
        assert!(search("q=x&country=DEU").is_err());
        let r = search("lat=47.1&lon=9.5&radius=500&country=li,AT&layers=house,street").unwrap();
        assert_eq!(r.countries, vec!["li", "at"]);
        assert_eq!(r.layers, vec![Layer::House, Layer::Street]);
        assert_eq!(r.radius_m, Some(500.0));
    }

    #[test]
    fn lookup_ids() {
        let parse = |q: &str| {
            let p: LookupParams = serde_urlencoded::from_str(q).unwrap();
            p.into_ids(&ApiConfig::default())
        };
        let (ids, _) = parse("osm_ids=N1,w22,R333").unwrap();
        assert_eq!(
            ids,
            vec![
                (OsmType::Node, 1),
                (OsmType::Way, 22),
                (OsmType::Relation, 333)
            ]
        );
        assert!(parse("osm_ids=X1").is_err());
        assert!(parse("osm_ids=Nabc").is_err());
        assert!(parse("").is_err());
    }
}
