//! Hits -> GeoJSON.

use geojson::{Feature, FeatureCollection, Geometry, JsonObject, JsonValue};
use geors_index::Hit;
use serde_json::json;

fn round(v: f64, decimals: i32) -> f64 {
    let f = 10f64.powi(decimals);
    (v * f).round() / f
}

fn put(props: &mut JsonObject, key: &str, value: Option<impl Into<JsonValue>>) {
    if let Some(v) = value {
        props.insert(key.to_string(), v.into());
    }
}

pub fn feature(hit: &Hit, lang: Option<&str>) -> Feature {
    let p = &hit.place;
    let addr = hit.address(lang);
    let mut props = JsonObject::new();
    put(&mut props, "name", p.localized_name(lang));
    put(&mut props, "housenumber", p.housenumber.as_deref());
    put(&mut props, "street", p.street.as_deref());
    put(&mut props, "postcode", p.postcode.as_deref());
    put(&mut props, "district", addr.district);
    put(&mut props, "city", addr.city);
    put(&mut props, "county", addr.county);
    put(&mut props, "state", addr.state);
    put(&mut props, "country", addr.country);
    props.insert("countrycode".into(), addr.country_code.into());
    props.insert("type".into(), p.layer.as_str().into());
    props.insert("osm_type".into(), p.osm_type.as_str().into());
    props.insert("osm_id".into(), p.osm_id.into());
    props.insert("osm_key".into(), p.osm_key.clone().into());
    props.insert("osm_value".into(), p.osm_value.clone().into());
    if let Some(e) = &p.extent {
        // [min_lon, min_lat, max_lon, max_lat]
        props.insert(
            "extent".into(),
            json!([e.min_lon, e.min_lat, e.max_lon, e.max_lat]),
        );
    }
    put(&mut props, "distance", hit.distance_m.map(|d| round(d, 1)));
    props.insert("score".into(), round(hit.score as f64, 4).into());
    props.insert("importance".into(), round(p.importance as f64, 3).into());

    Feature {
        geometry: Some(Geometry::new_point([
            round(p.center.lon, 7),
            round(p.center.lat, 7),
        ])),
        properties: Some(props),
        ..Default::default()
    }
}

pub fn collection(hits: &[Hit], lang: Option<&str>) -> FeatureCollection {
    FeatureCollection {
        features: hits.iter().map(|h| feature(h, lang)).collect(),
        ..Default::default()
    }
}
