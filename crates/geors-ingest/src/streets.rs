//! Merge street and waterway segments: OSM splits one street (or river)
//! into many ways; a geocoder should return it once. Segments are merged
//! when they share name (and, for streets, city) and lie close together.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::Path;

use geors_core::geom::{self, BBox, LonLat};
use geors_core::spill::{SpillReader, SpillWriter};
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

/// Grid cell size for clustering, degrees (~1 km).
const GRID_DEG: f64 = 0.01;

/// Connected components of boxes that overlap after expansion by
/// [`JOIN_DEG`]. Boxes are bucketed into a grid so each box is only
/// compared with its spatial neighbours: roughly linear instead of O(n²).
pub fn clusters(boxes: &[BBox]) -> Vec<Vec<usize>> {
    let expanded: Vec<BBox> = boxes.iter().map(expanded).collect();
    let mut parent: Vec<usize> = (0..boxes.len()).collect();
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    let cell = |v: f64| (v / GRID_DEG).floor() as i32;
    for (i, b) in expanded.iter().enumerate() {
        for cx in cell(b.min_lon)..=cell(b.max_lon) {
            for cy in cell(b.min_lat)..=cell(b.max_lat) {
                let bucket = grid.entry((cx, cy)).or_default();
                for &j in bucket.iter() {
                    if expanded[j].intersects(b) {
                        let (a, r) = (find(&mut parent, i), find(&mut parent, j));
                        parent[a] = r;
                    }
                }
                bucket.push(i);
            }
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..boxes.len() {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }
    groups.into_values().collect()
}

/// Merge a group of same-name, same-city street segments into one place
/// per spatial cluster.
pub fn merge_group(group: Vec<Place>) -> Vec<Place> {
    if group.len() <= 1 {
        return group;
    }
    let boxes: Vec<BBox> = group.iter().map(Place::bbox).collect();
    let mut slots: Vec<Option<Place>> = group.into_iter().map(Some).collect();
    clusters(&boxes)
        .into_iter()
        .map(|idx| merge_cluster(idx.into_iter().filter_map(|i| slots[i].take()).collect()))
        .collect()
}

/// (name, city, country, osm_key): streets merge per city; rivers and
/// streams (no city) along their whole length. `osm_key` keeps a street and
/// a stream with the same name apart.
type StreetKey = (String, Option<u32>, Option<String>, String);

fn street_key(p: &Place, city: Option<u32>) -> StreetKey {
    (
        p.name.clone().unwrap_or_default().to_lowercase(),
        city,
        p.country_code.clone(),
        p.osm_key.clone(),
    )
}

/// Linear features that OSM splits into many ways and that should be
/// returned once: named streets, and rivers / streams / canals.
pub fn is_mergeable(p: &Place) -> bool {
    p.layer == Layer::Street || (p.osm_key == "waterway" && !p.lines.is_empty())
}

/// In-memory merge, for small inputs. `city_of(place)` identifies the
/// locality a street belongs to.
pub fn merge(places: Vec<Place>, city_of: impl Fn(&Place) -> Option<u32>) -> Vec<Place> {
    let (streets, mut out): (Vec<Place>, Vec<Place>) =
        places.into_iter().partition(|p| p.layer == Layer::Street);
    let mut groups: HashMap<StreetKey, Vec<Place>> = HashMap::new();
    for s in streets {
        groups
            .entry(street_key(&s, city_of(&s)))
            .or_default()
            .push(s);
    }
    for (_, group) in groups {
        out.extend(merge_group(group));
    }
    out
}

/// Street segments spilled to disk during import, with a small in-memory
/// index (48 bytes per segment) used to group them for merging.
pub struct StreetStore {
    spill: SpillWriter,
    segs: Vec<(u64, Option<u32>, u64)>,
}

impl StreetStore {
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            spill: SpillWriter::create(path)?,
            segs: Vec::new(),
        })
    }

    pub fn len(&self) -> usize {
        self.segs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    pub fn push(&mut self, place: &Place, city: Option<u32>) -> io::Result<()> {
        let mut h = DefaultHasher::new();
        street_key(place, city).hash(&mut h);
        let offset = self.spill.push(place)?;
        self.segs.push((h.finish(), city, offset));
        Ok(())
    }

    /// Merge each group and hand the results to `emit`. Only one group of
    /// segments is in memory at a time. Returns (segments, merged streets).
    pub fn finish(
        mut self,
        mut emit: impl FnMut(Place) -> io::Result<()>,
    ) -> io::Result<(usize, usize)> {
        let spill = self.spill.finish()?;
        let reader = SpillReader::open(&spill)?;
        self.segs.sort_unstable_by_key(|s| (s.0, s.2));
        let (mut segments, mut streets) = (0, 0);
        for group in self.segs.chunk_by(|a, b| a.0 == b.0) {
            // Same hash: split by the real key in case of a collision.
            let mut by_key: HashMap<StreetKey, Vec<Place>> = HashMap::new();
            for &(_, city, offset) in group {
                let p = reader.get(offset)?;
                by_key.entry(street_key(&p, city)).or_default().push(p);
            }
            for (_, mut places) in by_key {
                // Deterministic merge result (identity, centre) regardless
                // of the order segments arrived in.
                places.sort_by_key(|p| p.osm_id);
                segments += places.len();
                for merged in merge_group(places) {
                    streets += 1;
                    emit(merged)?;
                }
            }
        }
        let _ = std::fs::remove_file(&spill.path);
        Ok((segments, streets))
    }
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
    fn grid_clusters_match_pairwise() {
        // Chains of touching boxes plus isolated ones, spread over many cells.
        let mut boxes = Vec::new();
        for chain in 0..50 {
            let lat = 47.0 + chain as f64 * 0.05;
            for k in 0..20 {
                let lon = 9.0 + k as f64 * 0.0015;
                boxes.push(BBox::new(lon, lat, lon + 0.001, lat + 0.0001));
            }
            boxes.push(BBox::new(10.0, lat, 10.0001, lat + 0.0001));
        }
        let mut got: Vec<Vec<usize>> = clusters(&boxes);
        for c in &mut got {
            c.sort();
        }
        got.sort();
        // Brute force reference.
        let exp: Vec<BBox> = boxes.iter().map(expanded).collect();
        let mut parent: Vec<usize> = (0..boxes.len()).collect();
        for i in 0..boxes.len() {
            for j in i + 1..boxes.len() {
                if exp[i].intersects(&exp[j]) {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    parent[a] = b;
                }
            }
        }
        let mut want: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..boxes.len() {
            let r = find(&mut parent, i);
            want.entry(r).or_default().push(i);
        }
        let mut want: Vec<Vec<usize>> = want.into_values().collect();
        want.sort();
        assert_eq!(got, want);
        assert_eq!(got.len(), 100);
    }

    #[test]
    fn store_merges_by_group() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = StreetStore::create(&dir.path().join("streets")).unwrap();
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
        let c = street(
            3,
            "Städtle",
            vec![LonLat::new(9.521, 47.14), LonLat::new(9.522, 47.141)],
        );
        store.push(&a, Some(1)).unwrap();
        store.push(&b, Some(1)).unwrap();
        // Same name, different city: separate street.
        store.push(&c, Some(2)).unwrap();
        let mut out = Vec::new();
        let (segments, streets) = store
            .finish(|p| {
                out.push(p);
                Ok(())
            })
            .unwrap();
        assert_eq!((segments, streets), (3, 2));
        assert!(
            out.iter()
                .any(|p| p.lines.len() == 2 && p.merged_ids.len() == 1)
        );
    }

    #[test]
    fn midpoint() {
        let m = line_midpoint(&[LonLat::new(0.0, 0.0), LonLat::new(0.02, 0.0)]).unwrap();
        assert!((m.lon - 0.01).abs() < 1e-9);
    }
}
