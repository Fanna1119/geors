//! Query-string parameters: parsed here, turned into library requests.
//!
//! Domain rules (coordinates in range, a radius needs a point, area-only
//! layers for `/contains`, ...) live in the `geors` request builders; this
//! module adds what is HTTP policy: limits and defaults from [`ApiConfig`].

use geors::geojson::Output;
use geors::{BBox, Contains, Layer, LonLat, Nearest, PlaceRef, Reverse, Search, TagFilter};
use serde::Deserialize;

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

/// `/contains`: which areas contain a point.
#[derive(Debug, Deserialize)]
pub struct ContainsParams {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub layers: Option<String>,
    pub country: Option<String>,
    pub lang: Option<String>,
    pub geometry: Option<String>,
}

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

/// `osm_tag` filters from the raw query string: repeated (Photon style,
/// `osm_tag=amenity:cafe&osm_tag=!amenity:parking`) or comma-separated.
pub fn osm_tags(raw: Option<&str>) -> Result<TagFilter, ApiError> {
    let values: Vec<String> = form_urlencoded::parse(raw.unwrap_or("").as_bytes())
        .filter(|(k, _)| k == "osm_tag")
        .flat_map(|(_, v)| v.split(',').map(str::to_string).collect::<Vec<_>>())
        .collect();
    TagFilter::parse(values.iter().map(String::as_str)).map_err(ApiError::bad_request)
}

/// `lat` and `lon` together, or neither. Range checks happen in the library.
fn point(lat: Option<f64>, lon: Option<f64>) -> Result<Option<LonLat>, ApiError> {
    match (lat, lon) {
        (None, None) => Ok(None),
        (Some(lat), Some(lon)) => Ok(Some(LonLat::new(lon, lat))),
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
        Some(r) if r > cfg.max_radius_m => Err(ApiError::bad_request(format!(
            "'radius' must be at most {} metres",
            cfg.max_radius_m
        ))),
        r => Ok(r),
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

fn countries(v: &Option<String>) -> Vec<&str> {
    comma_list(v).collect()
}

impl SearchParams {
    pub fn output(&self) -> Result<Output, ApiError> {
        output(&self.lang, &self.geometry)
    }

    pub fn into_search(self, cfg: &ApiConfig, tags: TagFilter) -> Result<Search, ApiError> {
        let focus = point(self.lat, self.lon)?;
        let bbox = self
            .bbox
            .as_deref()
            .map(BBox::parse)
            .transpose()
            .map_err(ApiError::bad_request)?;
        let query = self.q.as_deref().filter(|q| !q.trim().is_empty());
        let mut search = match (query, focus, bbox) {
            (Some(q), _, _) => Search::text(q),
            (None, Some(p), _) => Search::around(p),
            (None, None, Some(b)) => Search::within(b),
            (None, None, None) => {
                return Err(ApiError::bad_request(
                    "provide a query 'q', a location ('lat'/'lon'), or a 'bbox'",
                ));
            }
        };
        if let Some(p) = focus {
            search = search.near(p);
        }
        if let Some(r) = radius(self.radius, cfg)? {
            if focus.is_none() {
                return Err(ApiError::bad_request("'radius' requires 'lat' and 'lon'"));
            }
            search = search.radius_m(r);
        }
        if let Some(b) = bbox {
            search = search.bbox(b);
        }
        Ok(search
            .countries(countries(&self.country))
            .layers(layers(&self.layers)?)
            .limit(limit(self.limit, cfg.default_limit, cfg)?)
            .autocomplete(self.autocomplete.unwrap_or(true))
            .tags(tags))
    }
}

impl PointParams {
    pub fn output(&self) -> Result<Output, ApiError> {
        output(&self.lang, &self.geometry)
    }

    /// `/reverse`: what is at this point. Defaults to one result within
    /// the configured radius, restricted to address-like layers.
    pub fn into_reverse(self, cfg: &ApiConfig, tags: TagFilter) -> Result<Reverse, ApiError> {
        Ok(Reverse::at(required_point(self.lat, self.lon)?)
            .radius_m(radius(self.radius, cfg)?.unwrap_or(cfg.reverse_radius_m))
            .layers(layers(&self.layers)?)
            .countries(countries(&self.country))
            .limit(limit(self.limit, 1, cfg)?)
            .tags(tags))
    }

    /// `/nearest`: no default radius or layer filter.
    pub fn into_nearest(self, cfg: &ApiConfig, tags: TagFilter) -> Result<Nearest, ApiError> {
        let mut nearest = Nearest::to(required_point(self.lat, self.lon)?)
            .layers(layers(&self.layers)?)
            .countries(countries(&self.country))
            .limit(limit(self.limit, 5, cfg)?)
            .tags(tags);
        if let Some(r) = radius(self.radius, cfg)? {
            nearest = nearest.max_distance_m(r);
        }
        Ok(nearest)
    }
}

impl ContainsParams {
    pub fn output(&self) -> Result<Output, ApiError> {
        output(&self.lang, &self.geometry)
    }

    pub fn into_contains(self) -> Result<Contains, ApiError> {
        Ok(Contains::at(required_point(self.lat, self.lon)?)
            .layers(layers(&self.layers)?)
            .countries(countries(&self.country)))
    }
}

impl LookupParams {
    pub fn output(&self) -> Result<Output, ApiError> {
        output(&self.lang, &self.geometry)
    }

    /// `osm_ids=N123,W456,R789` (OSM ids) and `P123` (Nominatim
    /// `place_id`). Returns the ids and the country filter.
    pub fn into_ids(self, cfg: &ApiConfig) -> Result<(Vec<PlaceRef>, Vec<String>), ApiError> {
        let ids: Vec<PlaceRef> = comma_list(&self.osm_ids)
            .map(|s| s.parse().map_err(ApiError::bad_request))
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
        let countries = countries(&self.country)
            .into_iter()
            .map(String::from)
            .collect();
        Ok((ids, countries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(q: &str) -> Result<Search, ApiError> {
        let p: SearchParams = serde_urlencoded::from_str(q).unwrap();
        p.into_search(&ApiConfig::default(), TagFilter::default())
    }

    #[test]
    fn http_policy() {
        assert!(search("q=vaduz").is_ok());
        assert!(search("").is_err());
        assert!(search("q=x&lat=47").is_err());
        assert!(search("q=x&limit=1000").is_err());
        assert!(search("q=x&layers=city,planet").is_err());
        assert!(search("q=x&lat=47.1&lon=9.5&radius=600000").is_err());
        assert!(search("lat=47.1&lon=9.5&radius=500&country=li,AT&layers=house,street").is_ok());
    }

    #[test]
    fn osm_tag_values() {
        let f = osm_tags(Some(
            "q=x&osm_tag=amenity:cafe&osm_tag=!amenity:parking,shop",
        ))
        .unwrap();
        assert!(f.matches("amenity", "cafe") && f.matches("shop", "bakery"));
        assert!(!f.matches("amenity", "parking") && !f.matches("highway", "primary"));
        assert!(osm_tags(Some("q=x")).unwrap().is_empty());
        assert!(osm_tags(Some("osm_tag=%3A")).is_err());
    }

    #[test]
    fn lookup_ids() {
        let parse = |q: &str| {
            let p: LookupParams = serde_urlencoded::from_str(q).unwrap();
            p.into_ids(&ApiConfig::default())
        };
        let (ids, _) = parse("osm_ids=N1,w22,R333,P44").unwrap();
        assert_eq!(
            ids,
            vec![
                PlaceRef::Osm(geors::OsmType::Node, 1),
                PlaceRef::Osm(geors::OsmType::Way, 22),
                PlaceRef::Osm(geors::OsmType::Relation, 333),
                PlaceRef::Nominatim(44),
            ]
        );
        assert!(parse("osm_ids=é1").is_err());
        assert!(parse("osm_ids=X1").is_err());
        assert!(parse("osm_ids=Nabc").is_err());
        assert!(parse("").is_err());
    }
}
