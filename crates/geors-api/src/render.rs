//! Hits -> GeoJSON.

use geojson::{
    Feature, FeatureCollection, Geometry, GeometryValue, JsonObject, JsonValue, Position,
};
use geors_core::LonLat;
use geors_index::{Hit, Shape};
use serde_json::json;

/// Response options shared by all endpoints.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub lang: Option<String>,
    /// `geometry=full`: return lines / polygons instead of the centre point.
    pub full_geometry: bool,
}

fn round(v: f64, decimals: i32) -> f64 {
    let f = 10f64.powi(decimals);
    (v * f).round() / f
}

fn pos(p: &LonLat) -> Position {
    Position::from([round(p.lon, 7), round(p.lat, 7)])
}

fn put(props: &mut JsonObject, key: &str, value: Option<impl Into<JsonValue>>) {
    if let Some(v) = value {
        props.insert(key.to_string(), v.into());
    }
}

fn geometry(shape: Shape) -> Geometry {
    let line = |l: &[LonLat]| l.iter().map(pos).collect::<Vec<_>>();
    let poly = |rings: &[Vec<LonLat>]| rings.iter().map(|r| line(r)).collect::<Vec<_>>();
    Geometry::new(match shape {
        Shape::Point(p) => GeometryValue::new_point(pos(&p)),
        Shape::Lines(lines) if lines.len() == 1 => GeometryValue::new_line_string(line(&lines[0])),
        Shape::Lines(lines) => GeometryValue::new_multi_line_string(lines.iter().map(|l| line(l))),
        Shape::Polygons(polys) if polys.len() == 1 => GeometryValue::new_polygon(poly(&polys[0])),
        Shape::Polygons(polys) => GeometryValue::new_multi_polygon(polys.iter().map(|p| poly(p))),
    })
}

pub fn feature(hit: &Hit, out: &Output) -> Feature {
    let lang = out.lang.as_deref();
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

    let geometry = if out.full_geometry {
        props.insert(
            "center".into(),
            json!([round(p.center.lon, 7), round(p.center.lat, 7)]),
        );
        geometry(hit.shape())
    } else {
        geometry(Shape::Point(p.center))
    };
    Feature {
        geometry: Some(geometry),
        properties: Some(props),
        ..Default::default()
    }
}

pub fn collection(hits: &[Hit], out: &Output) -> FeatureCollection {
    FeatureCollection {
        features: hits.iter().map(|h| feature(h, out)).collect(),
        ..Default::default()
    }
}
