//! House-number interpolation (`addr:interpolation` ways).
//!
//! Mappers often tag only some houses and draw an interpolation line
//! between them: a way from the node of no. 1 to the node of no. 15 with
//! `addr:interpolation=odd` stands for 3, 5, ..., 13. The numbers in between
//! are spread evenly along the line, between each pair of consecutive
//! numbered nodes (a line may have numbered nodes in the middle too).

use geors_core::geom::{self, LonLat};
use geors_core::{Layer, OsmType, Place};

/// Which numbers a line stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Odd,
    Even,
    All,
}

impl Kind {
    /// The `addr:interpolation` value; `None` for unsupported ones
    /// (`alphabetic`, other step sizes).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "odd" => Some(Kind::Odd),
            "even" => Some(Kind::Even),
            "all" | "1" => Some(Kind::All),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Kind::Odd => "odd",
            Kind::Even => "even",
            Kind::All => "all",
        }
    }

    fn step(self) -> u32 {
        if self == Kind::All { 1 } else { 2 }
    }

    fn accepts(self, n: u32) -> bool {
        match self {
            Kind::Odd => n % 2 == 1,
            Kind::Even => n.is_multiple_of(2),
            Kind::All => true,
        }
    }
}

/// An interpolation line from pass 2.
#[derive(Debug, Clone)]
pub struct Line {
    pub way: i64,
    pub refs: Vec<i64>,
    pub kind: Kind,
}

/// Address of a numbered node on a line (from pass 5).
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub number: u32,
    pub street: Option<String>,
    pub postcode: Option<String>,
    pub city: Option<String>,
}

impl Endpoint {
    /// From a house place; only plain numbers ("12", not "12a") count.
    pub fn of(place: &Place) -> Option<Self> {
        let hn = place.housenumber.as_deref()?.trim();
        if hn.is_empty() || !hn.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Endpoint {
            number: hn.parse().ok()?,
            street: place.street.clone(),
            postcode: place.postcode.clone(),
            city: place.city.clone(),
        })
    }
}

/// Longest gap that is filled (larger ones are mapping errors).
const MAX_GAP: u32 = 1000;

/// The houses a line stands for. `pts` are the line's coordinates (one per
/// ref), `endpoint(node)` the address of a numbered node.
pub fn houses<'a>(
    line: &Line,
    pts: &[LonLat],
    endpoint: impl Fn(i64) -> Option<&'a Endpoint>,
) -> Vec<Place> {
    if pts.len() != line.refs.len() || pts.len() < 2 {
        return Vec::new();
    }
    // Distance along the line at each vertex.
    let mut along = vec![0.0f64; pts.len()];
    for i in 1..pts.len() {
        along[i] = along[i - 1] + geom::haversine(pts[i - 1], pts[i]);
    }
    let numbered: Vec<(usize, &Endpoint)> = line
        .refs
        .iter()
        .enumerate()
        .filter_map(|(i, &r)| endpoint(r).map(|e| (i, e)))
        .collect();
    let mut out = Vec::new();
    for pair in numbered.windows(2) {
        let ((i, a), (j, b)) = (pair[0], pair[1]);
        // Both ends must be on the same street, and of the line's kind.
        if a.street.is_none() || a.street != b.street || a.number == b.number {
            continue;
        }
        if !line.kind.accepts(a.number) || !line.kind.accepts(b.number) {
            continue;
        }
        let (lo, hi) = (a.number.min(b.number), a.number.max(b.number));
        if hi - lo > MAX_GAP {
            continue;
        }
        let mut n = lo + line.kind.step();
        while n < hi {
            // Fraction of the way from `a` to `b` (numbers may descend).
            let t = if b.number > a.number {
                (n - a.number) as f64 / (b.number - a.number) as f64
            } else {
                (a.number - n) as f64 / (a.number - b.number) as f64
            };
            let at = along[i] + t * (along[j] - along[i]);
            out.push(house(line, n, point_at(pts, &along, at), a));
            n += line.kind.step();
        }
    }
    out
}

/// The point `d` metres along the line.
fn point_at(pts: &[LonLat], along: &[f64], d: f64) -> LonLat {
    let k = along.partition_point(|&x| x < d).clamp(1, pts.len() - 1);
    let seg = along[k] - along[k - 1];
    let t = if seg > 0.0 {
        ((d - along[k - 1]) / seg).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (p, q) = (pts[k - 1], pts[k]);
    LonLat::new(p.lon + t * (q.lon - p.lon), p.lat + t * (q.lat - p.lat))
}

fn house(line: &Line, number: u32, center: LonLat, from: &Endpoint) -> Place {
    Place {
        osm_type: OsmType::Way,
        osm_id: line.way,
        osm_key: "addr:interpolation".into(),
        osm_value: line.kind.as_str().into(),
        layer: Layer::House,
        name: None,
        names: Default::default(),
        alt_names: Vec::new(),
        housenumber: Some(number.to_string()),
        street: from.street.clone(),
        postcode: from.postcode.clone(),
        city: from.city.clone(),
        parents: Vec::new(),
        country_code: None,
        center,
        extent: None,
        importance: Layer::House.base_importance(),
        lines: Vec::new(),
        polygons: Vec::new(),
        merged_ids: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ep(number: u32, street: &str) -> Endpoint {
        Endpoint {
            number,
            street: Some(street.into()),
            postcode: Some("9490".into()),
            city: None,
        }
    }

    fn numbers(places: &[Place]) -> Vec<u32> {
        places
            .iter()
            .map(|p| p.housenumber.as_ref().unwrap().parse().unwrap())
            .collect()
    }

    #[test]
    fn fills_odd_numbers_evenly() {
        let line = Line {
            way: 7,
            refs: vec![1, 2, 3],
            kind: Kind::Odd,
        };
        // 0.0 -> 0.001 -> 0.002 (two equal segments).
        let pts = [
            LonLat::new(9.0, 47.0),
            LonLat::new(9.001, 47.0),
            LonLat::new(9.002, 47.0),
        ];
        let ends: HashMap<i64, Endpoint> = [(1, ep(1, "Städtle")), (3, ep(9, "Städtle"))].into();
        let out = houses(&line, &pts, |id| ends.get(&id));
        assert_eq!(numbers(&out), vec![3, 5, 7]);
        // 5 is halfway.
        assert!((out[1].center.lon - 9.001).abs() < 1e-9);
        assert!(
            out.iter()
                .all(|p| p.street.as_deref() == Some("Städtle") && p.osm_id == 7)
        );
        assert_eq!(out[0].osm_value, "odd");
        assert_eq!(out[0].postcode.as_deref(), Some("9490"));
    }

    #[test]
    fn descending_and_middle_nodes() {
        let line = Line {
            way: 1,
            refs: vec![1, 2, 3],
            kind: Kind::Even,
        };
        let pts = [
            LonLat::new(0.0, 0.0),
            LonLat::new(0.001, 0.0),
            LonLat::new(0.002, 0.0),
        ];
        let ends: HashMap<i64, Endpoint> =
            [(1, ep(10, "A")), (2, ep(6, "A")), (3, ep(2, "A"))].into();
        let out = houses(&line, &pts, |id| ends.get(&id));
        assert_eq!(numbers(&out), vec![8, 4]);
        // 8 is halfway between 10 (at 0) and 6 (at 0.001).
        assert!((out[0].center.lon - 0.0005).abs() < 1e-9);
    }

    #[test]
    fn skips_inconsistent_lines() {
        let line = Line {
            way: 1,
            refs: vec![1, 2],
            kind: Kind::Odd,
        };
        let pts = [LonLat::new(0.0, 0.0), LonLat::new(0.001, 0.0)];
        let check = |a: Endpoint, b: Endpoint| {
            let ends: HashMap<i64, Endpoint> = [(1, a), (2, b)].into();
            houses(&line, &pts, |id| ends.get(&id)).len()
        };
        assert_eq!(check(ep(1, "A"), ep(2, "A")), 0, "even end on an odd line");
        assert_eq!(check(ep(1, "A"), ep(9, "B")), 0, "different streets");
        assert_eq!(check(ep(1, "A"), ep(5001, "A")), 0, "gap too large");
        assert_eq!(check(ep(1, "A"), ep(5, "A")), 1);
        // Missing coordinates.
        let ends: HashMap<i64, Endpoint> = [(1, ep(1, "A")), (2, ep(5, "A"))].into();
        assert!(houses(&line, &pts[..1], |id| ends.get(&id)).is_empty());
    }

    #[test]
    fn parses_kinds_and_numbers() {
        assert_eq!(Kind::parse("odd"), Some(Kind::Odd));
        assert_eq!(Kind::parse("all"), Some(Kind::All));
        assert_eq!(Kind::parse("alphabetic"), None);
        let mut p = house(
            &Line {
                way: 1,
                refs: vec![],
                kind: Kind::All,
            },
            5,
            LonLat::new(0.0, 0.0),
            &ep(1, "A"),
        );
        assert_eq!(Endpoint::of(&p).unwrap().number, 5);
        p.housenumber = Some("12a".into());
        assert!(Endpoint::of(&p).is_none());
    }
}
