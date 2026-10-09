//! Merge street segments: OSM splits one street into many ways; a geocoder
//! should return it once. Segments are merged when they share name and
//! city and lie close to each other.

use std::collections::HashMap;

use geors_core::geom::{self, BBox, LonLat};
use geors_core::{Layer, Place};

/// Segments whose bboxes are within roughly this distance are connected.
const JOIN_DEG: f64 = 0.001; // ~110 m

fn expanded(b: &BBox) -> BBox {
    let k = b.center().lat.to_radians().cos().max(0.1);
    BBox::new(
        b.min_lon - JOIN_DEG / k,
        b.min_lat - JOIN_DEG,
        b.max_lon + JOIN_DEG / k,
        b.max_lat + JOIN_DEG,
    )
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// Point at half the length of a polyline.
pub fn line_midpoint(line: &[LonLat]) -> Option<LonLat> {
    let total: f64 = line.windows(2).map(|w| geom::haversine(w[0], w[1])).sum();
    let mut remaining = total / 2.0;
    for w in line.windows(2) {
        let d = geom::haversine(w[0], w[1]);
        if d >= remaining && d > 0.0 {
            let t = remaining / d;
            return Some(LonLat::new(
                w[0].lon + t * (w[1].lon - w[0].lon),
                w[0].lat + t * (w[1].lat - w[0].lat),
            ));
        }
        remaining -= d;
    }
    line.first().copied()
}

/// `city_of(place)` identifies the locality a street belongs to.
pub fn merge(places: Vec<Place>, city_of: impl Fn(&Place) -> Option<u32>) -> Vec<Place> {
    let (streets, mut out): (Vec<Place>, Vec<Place>) =
        places.into_iter().partition(|p| p.layer == Layer::Street);
    let mut groups: HashMap<(String, Option<u32>, Option<String>), Vec<Place>> = HashMap::new();
    for s in streets {
        let key = (
            s.name.clone().unwrap_or_default().to_lowercase(),
            city_of(&s),
            s.country_code.clone(),
        );
        groups.entry(key).or_default().push(s);
    }
    for (_, group) in groups {
        if group.len() == 1 {
            out.extend(group);
            continue;
        }
        let boxes: Vec<BBox> = group.iter().map(|p| expanded(&p.bbox())).collect();
        let mut parent: Vec<usize> = (0..group.len()).collect();
        for i in 0..group.len() {
            for j in i + 1..group.len() {
                if boxes[i].intersects(&boxes[j]) {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    parent[a] = b;
                }
            }
        }
        let mut clusters: HashMap<usize, Vec<Place>> = HashMap::new();
        for (i, p) in group.into_iter().enumerate() {
            let root = find(&mut parent, i);
            clusters.entry(root).or_default().push(p);
        }
        out.extend(clusters.into_values().map(merge_cluster));
    }
    out
}

fn merge_cluster(mut parts: Vec<Place>) -> Place {
    if parts.len() == 1 {
        return parts.pop().unwrap();
    }
    // The most important (then longest) segment provides identity and tags.
    parts.sort_by(|a, b| {
        b.importance.total_cmp(&a.importance).then(
            b.lines
                .iter()
                .map(Vec::len)
                .sum::<usize>()
                .cmp(&a.lines.iter().map(Vec::len).sum()),
        )
    });
    let mut bbox = BBox::empty();
    for p in &parts {
        bbox.union(&p.bbox());
    }
    let center = bbox.center();
    let mut merged = parts.remove(0);
    for p in parts {
        merged.merged_ids.push(p.osm_id);
        merged.merged_ids.extend(p.merged_ids);
        merged.lines.extend(p.lines);
        for (lang, n) in p.names {
            merged.names.entry(lang).or_insert(n);
        }
        merged.alt_names.extend(p.alt_names);
        if merged.postcode.is_none() {
            merged.postcode = p.postcode;
        }
    }
    merged.alt_names.sort();
    merged.alt_names.dedup();
    // Representative point: the segment midpoint closest to the overall centre.
    if let Some(mid) = merged
        .lines
        .iter()
        .filter_map(|l| line_midpoint(l))
        .min_by(|a, b| geom::haversine(*a, center).total_cmp(&geom::haversine(*b, center)))
    {
        merged.center = mid;
    }
    merged.extent = Some(bbox);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use geors_core::OsmType;

    fn street(id: i64, name: &str, line: Vec<LonLat>) -> Place {
        Place {
            osm_type: OsmType::Way,
            osm_id: id,
            osm_key: "highway".into(),
            osm_value: "residential".into(),
            layer: Layer::Street,
            name: Some(name.into()),
            names: Default::default(),
            alt_names: vec![],
            housenumber: None,
            street: None,
            postcode: None,
            city: None,
            parents: vec![],
            country_code: Some("LI".into()),
            center: line[0],
            extent: None,
            importance: 0.25,
            lines: vec![line],
            polygons: Vec::new(),
            merged_ids: Vec::new(),
        }
    }

    #[test]
    fn merges_adjacent_segments_only() {
        let a = street(
            1,
            "Städtle",
            vec![LonLat::new(9.52, 47.14), LonLat::new(9.521, 47.14)],
        );
        let b = street(
            2,
            "Städtle",
            vec![LonLat::new(9.521, 47.14), LonLat::new(9.522, 47.141)],
        );
        let far = street(
            3,
            "Städtle",
            vec![LonLat::new(9.60, 47.20), LonLat::new(9.601, 47.20)],
        );
        let other = street(
            4,
            "Aeulestrasse",
            vec![LonLat::new(9.52, 47.14), LonLat::new(9.521, 47.14)],
        );
        let out = merge(vec![a, b, far, other], |_| None);
        assert_eq!(out.len(), 3);
        let merged = out
            .iter()
            .find(|p| p.lines.len() == 2)
            .expect("merged street");
        assert!(merged.extent.is_some());
        assert_eq!(merged.merged_ids.len(), 1);
    }

    #[test]
    fn midpoint() {
        let m = line_midpoint(&[LonLat::new(0.0, 0.0), LonLat::new(0.02, 0.0)]).unwrap();
        assert!((m.lon - 0.01).abs() < 1e-9);
    }
}
