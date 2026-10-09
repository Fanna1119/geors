//! Small, dependency-free spherical geometry helpers.
//!
//! All coordinates are WGS84 `(lon, lat)` in degrees, distances in metres.

use serde::{Deserialize, Serialize};

pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LonLat {
    pub lon: f64,
    pub lat: f64,
}

impl LonLat {
    pub fn new(lon: f64, lat: f64) -> Self {
        Self { lon, lat }
    }

    pub fn is_valid(&self) -> bool {
        self.lon.is_finite()
            && self.lat.is_finite()
            && (-180.0..=180.0).contains(&self.lon)
            && (-90.0..=90.0).contains(&self.lat)
    }
}

/// Great-circle distance in metres.
pub fn haversine(a: LonLat, b: LonLat) -> f64 {
    let (lat1, lat2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b.lon - a.lon).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().min(1.0).asin()
}

/// Distance in metres from `p` to the segment `a`-`b`.
///
/// Uses a local equirectangular projection centred on `p`, which is accurate
/// to well below a metre for the segment lengths found in OSM ways.
pub fn point_segment_distance(p: LonLat, a: LonLat, b: LonLat) -> f64 {
    let k = p.lat.to_radians().cos();
    let project = |q: LonLat| ((q.lon - p.lon) * k, q.lat - p.lat);
    let (ax, ay) = project(a);
    let (bx, by) = project(b);
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (-(ax * dx + ay * dy) / len2).clamp(0.0, 1.0)
    };
    let closest = LonLat::new(p.lon + (ax + t * dx) / k, p.lat + ay + t * dy);
    haversine(p, closest)
}

/// Distance in metres from `p` to a polyline. Returns `None` for an empty line.
pub fn point_line_distance(p: LonLat, line: &[LonLat]) -> Option<f64> {
    match line {
        [] => None,
        [only] => Some(haversine(p, *only)),
        _ => line
            .windows(2)
            .map(|w| point_segment_distance(p, w[0], w[1]))
            .min_by(f64::total_cmp),
    }
}

/// Axis aligned bounding box in degrees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

impl BBox {
    pub fn new(min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) -> Self {
        Self {
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        }
    }

    pub fn point(p: LonLat) -> Self {
        Self::new(p.lon, p.lat, p.lon, p.lat)
    }

    pub fn empty() -> Self {
        Self::new(
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        )
    }

    pub fn is_empty(&self) -> bool {
        self.min_lon > self.max_lon || self.min_lat > self.max_lat
    }

    pub fn extend(&mut self, p: LonLat) {
        self.min_lon = self.min_lon.min(p.lon);
        self.min_lat = self.min_lat.min(p.lat);
        self.max_lon = self.max_lon.max(p.lon);
        self.max_lat = self.max_lat.max(p.lat);
    }

    pub fn union(&mut self, o: &BBox) {
        self.min_lon = self.min_lon.min(o.min_lon);
        self.min_lat = self.min_lat.min(o.min_lat);
        self.max_lon = self.max_lon.max(o.max_lon);
        self.max_lat = self.max_lat.max(o.max_lat);
    }

    pub fn contains(&self, p: LonLat) -> bool {
        p.lon >= self.min_lon
            && p.lon <= self.max_lon
            && p.lat >= self.min_lat
            && p.lat <= self.max_lat
    }

    pub fn intersects(&self, o: &BBox) -> bool {
        self.min_lon <= o.max_lon
            && self.max_lon >= o.min_lon
            && self.min_lat <= o.max_lat
            && self.max_lat >= o.min_lat
    }

    pub fn center(&self) -> LonLat {
        LonLat::new(
            (self.min_lon + self.max_lon) / 2.0,
            (self.min_lat + self.max_lat) / 2.0,
        )
    }

    /// A box that is guaranteed to contain every point within `radius_m` of
    /// `center`. Clamped to valid coordinates; crossing the antimeridian is
    /// handled by widening to the full longitude range.
    pub fn around(center: LonLat, radius_m: f64) -> Self {
        let dlat = (radius_m / EARTH_RADIUS_M).to_degrees();
        let min_lat = (center.lat - dlat).max(-90.0);
        let max_lat = (center.lat + dlat).min(90.0);
        // Use the latitude closest to a pole, where a degree of longitude is shortest.
        let extreme = min_lat.abs().max(max_lat.abs());
        let cos = extreme.to_radians().cos();
        let dlon = if cos < 1e-9 { 360.0 } else { dlat / cos };
        if dlon >= 180.0 || center.lon - dlon < -180.0 || center.lon + dlon > 180.0 {
            return Self::new(-180.0, min_lat, 180.0, max_lat);
        }
        Self::new(center.lon - dlon, min_lat, center.lon + dlon, max_lat)
    }

    /// Parse `min_lon,min_lat,max_lon,max_lat`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<f64> = s
            .split(',')
            .map(|p| p.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .map_err(|_| {
                format!("bbox '{s}' must be four numbers: min_lon,min_lat,max_lon,max_lat")
            })?;
        let [min_lon, min_lat, max_lon, max_lat] = parts[..] else {
            return Err(format!(
                "bbox '{s}' must have exactly four values: min_lon,min_lat,max_lon,max_lat"
            ));
        };
        let b = Self::new(min_lon, min_lat, max_lon, max_lat);
        if !LonLat::new(min_lon, min_lat).is_valid() || !LonLat::new(max_lon, max_lat).is_valid() {
            return Err(format!("bbox '{s}' has coordinates out of range"));
        }
        if b.is_empty() {
            return Err(format!("bbox '{s}' has min greater than max"));
        }
        Ok(b)
    }
}

/// Fixed point coordinate (1e-7 degrees, about 1 cm), as used by OSM itself.
pub fn to_e7(v: f64) -> i32 {
    (v * 1e7).round() as i32
}

pub fn from_e7(v: i32) -> f64 {
    v as f64 / 1e7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_known_distance() {
        // Vaduz -> Schaan is roughly 4.6 km.
        let vaduz = LonLat::new(9.5209, 47.1410);
        let schaan = LonLat::new(9.5095, 47.1650);
        let d = haversine(vaduz, schaan);
        assert!((2_500.0..3_000.0).contains(&d), "{d}");
        assert_eq!(haversine(vaduz, vaduz), 0.0);
    }

    #[test]
    fn segment_distance() {
        let a = LonLat::new(0.0, 0.0);
        let b = LonLat::new(0.01, 0.0);
        let p = LonLat::new(0.005, 0.001);
        let d = point_segment_distance(p, a, b);
        assert!((d - 111.2).abs() < 1.0, "{d}");
        // Beyond the end of the segment the distance is to the endpoint.
        let q = LonLat::new(0.02, 0.0);
        assert!((point_segment_distance(q, a, b) - haversine(q, b)).abs() < 0.01);
    }

    #[test]
    fn bbox_around_contains_circle() {
        let c = LonLat::new(9.5, 47.1);
        let b = BBox::around(c, 1_000.0);
        for bearing in 0..36 {
            let t = (bearing as f64 * 10.0).to_radians();
            let p = LonLat::new(
                c.lon + (999.0 / EARTH_RADIUS_M).to_degrees() * t.sin() / c.lat.to_radians().cos(),
                c.lat + (999.0 / EARTH_RADIUS_M).to_degrees() * t.cos(),
            );
            assert!(b.contains(p));
        }
    }

    #[test]
    fn bbox_parse() {
        assert!(BBox::parse("9.4,47.0,9.6,47.3").is_ok());
        assert!(BBox::parse("9.6,47.0,9.4,47.3").is_err());
        assert!(BBox::parse("9.4,47.0,9.6").is_err());
        assert!(BBox::parse("a,b,c,d").is_err());
        assert!(BBox::parse("0,0,200,1").is_err());
    }
}
