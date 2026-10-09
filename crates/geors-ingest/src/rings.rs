//! Multipolygon assembly: join member ways into closed rings and build
//! (multi)polygons with holes.

use geo::{Coord, Intersects, LineString, MultiPolygon, Polygon};
use geors_core::LonLat;

use crate::reader::NodeCoords;

/// Join way node lists end-to-end into closed rings (by shared node ids).
/// Returns the rings and the number of open chains that could not be closed
/// (typically boundaries clipped at the edge of an extract).
pub fn join_rings(segments: Vec<&[i64]>) -> (Vec<Vec<i64>>, usize) {
    let mut open: Vec<Vec<i64>> = segments
        .into_iter()
        .filter(|s| s.len() >= 2)
        .map(<[i64]>::to_vec)
        .collect();
    let mut rings = Vec::new();
    let mut broken = 0;
    while let Some(mut cur) = open.pop() {
        loop {
            if cur.len() >= 4 && cur.first() == cur.last() {
                rings.push(cur);
                break;
            }
            let touches = |s: &Vec<i64>, id: i64| s[0] == id || *s.last().unwrap() == id;
            let (first, last) = (cur[0], *cur.last().unwrap());
            if let Some(i) = open.iter().position(|s| touches(s, last)) {
                let mut next = open.swap_remove(i);
                if next[0] != last {
                    next.reverse();
                }
                cur.extend_from_slice(&next[1..]);
            } else if let Some(i) = open.iter().position(|s| touches(s, first)) {
                let mut prev = open.swap_remove(i);
                if *prev.last().unwrap() != first {
                    prev.reverse();
                }
                prev.extend_from_slice(&cur[1..]);
                cur = prev;
            } else {
                broken += 1;
                break;
            }
        }
    }
    (rings, broken)
}

pub fn to_linestring(points: &[LonLat]) -> LineString<f64> {
    LineString::new(
        points
            .iter()
            .map(|p| Coord { x: p.lon, y: p.lat })
            .collect(),
    )
}

fn ring_coords(ring: &[i64], coords: &NodeCoords) -> Option<LineString<f64>> {
    let pts = coords.line(ring);
    // Every node must be present, otherwise the ring is not closed.
    (pts.len() == ring.len() && pts.len() >= 4).then(|| to_linestring(&pts))
}

/// Build a multipolygon from outer and inner member ways (as node lists).
pub fn assemble(
    outer: Vec<&[i64]>,
    inner: Vec<&[i64]>,
    coords: &NodeCoords,
) -> (Option<MultiPolygon<f64>>, usize) {
    let (outer_rings, broken_outer) = join_rings(outer);
    let (inner_rings, broken_inner) = join_rings(inner);
    let mut polygons: Vec<Polygon<f64>> = outer_rings
        .iter()
        .filter_map(|r| ring_coords(r, coords))
        .map(|ls| Polygon::new(ls, vec![]))
        .collect();
    for hole in inner_rings.iter().filter_map(|r| ring_coords(r, coords)) {
        let probe = hole.0[0];
        // A hole belongs to the outer ring it lies in (it may touch it).
        let shell = |p: &Polygon<f64>| Polygon::new(p.exterior().clone(), vec![]);
        if let Some(poly) = polygons.iter_mut().find(|p| shell(p).intersects(&probe)) {
            poly.interiors_push(hole);
        }
    }
    let broken = broken_outer + broken_inner;
    if polygons.is_empty() {
        (None, broken)
    } else {
        (Some(MultiPolygon::new(polygons)), broken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_reversed_segments() {
        let a = [1, 2, 3];
        let b = [5, 4, 3]; // reversed direction
        let c = [5, 6, 1];
        let (rings, broken) = join_rings(vec![&a, &b, &c]);
        assert_eq!(broken, 0);
        assert_eq!(rings.len(), 1);
        let r = &rings[0];
        assert_eq!(r.first(), r.last());
        assert_eq!(r.len(), 7);
    }

    #[test]
    fn reports_broken_chain() {
        let a = [1, 2, 3];
        let b = [3, 4];
        let (rings, broken) = join_rings(vec![&a, &b]);
        assert!(rings.is_empty());
        assert_eq!(broken, 1);
    }
}
