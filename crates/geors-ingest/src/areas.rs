//! Address hierarchy lookup: which admin areas contain a point, plus a
//! nearest-place-node fallback for cities/districts without boundaries.

use std::collections::HashMap;

use geo::{Contains, Coord, Intersects, MultiPolygon, Rect};
use geo_index::rtree::sort::HilbertSort;
use geo_index::rtree::{RTree, RTreeBuilder, RTreeIndex};
use geors_core::Layer;
use geors_core::geom::{self, BBox, LonLat};

/// Size of the grid cells used to cache point-in-polygon results, degrees.
const CELL: f64 = 0.02;

pub struct Area {
    pub unit: u32,
    pub layer: Layer,
    /// admin_level, or 11 for `place=*` areas (more specific than boundaries).
    pub rank: u8,
    pub polygon: MultiPolygon<f64>,
    pub bbox: BBox,
    pub country_code: Option<String>,
}

#[derive(Clone, Copy)]
enum Cell {
    Inside,
    Outside,
    Mixed,
}

/// Point-in-polygon index over admin areas.
///
/// Results are cached per (grid cell, area): a cell that no ring crosses is
/// entirely inside or outside, so only points in cells on a boundary pay for
/// an exact test. This keeps large country polygons cheap.
pub struct AreaIndex {
    pub areas: Vec<Area>,
    tree: Option<RTree<f64>>,
    cache: HashMap<(i32, i32, u32), Cell>,
}

impl AreaIndex {
    pub fn new(areas: Vec<Area>) -> Self {
        let tree = (!areas.is_empty()).then(|| {
            let mut b = RTreeBuilder::<f64>::new(areas.len() as u32);
            for a in &areas {
                b.add(
                    a.bbox.min_lon,
                    a.bbox.min_lat,
                    a.bbox.max_lon,
                    a.bbox.max_lat,
                );
            }
            b.finish::<HilbertSort>()
        });
        Self {
            areas,
            tree,
            cache: HashMap::new(),
        }
    }

    fn cell_state(&mut self, cx: i32, cy: i32, area: u32) -> Cell {
        if let Some(c) = self.cache.get(&(cx, cy, area)) {
            return *c;
        }
        let a = &self.areas[area as usize];
        let rect = Rect::new(
            Coord {
                x: cx as f64 * CELL,
                y: cy as f64 * CELL,
            },
            Coord {
                x: (cx + 1) as f64 * CELL,
                y: (cy + 1) as f64 * CELL,
            },
        );
        let crosses = a.polygon.iter().any(|p| {
            p.exterior().intersects(&rect) || p.interiors().iter().any(|r| r.intersects(&rect))
        });
        let state = if crosses {
            Cell::Mixed
        } else if a.polygon.contains(&rect.center()) {
            Cell::Inside
        } else {
            Cell::Outside
        };
        self.cache.insert((cx, cy, area), state);
        state
    }

    /// Indices of all areas containing `p`.
    pub fn containing(&mut self, p: LonLat) -> Vec<usize> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let ids = tree.search(p.lon, p.lat, p.lon, p.lat);
        let (cx, cy) = ((p.lon / CELL).floor() as i32, (p.lat / CELL).floor() as i32);
        let coord = Coord { x: p.lon, y: p.lat };
        ids.into_iter()
            .filter(|&i| match self.cell_state(cx, cy, i) {
                Cell::Inside => true,
                Cell::Outside => false,
                Cell::Mixed => self.areas[i as usize].polygon.contains(&coord),
            })
            .map(|i| i as usize)
            .collect()
    }
}

pub struct LocalityPoint {
    pub unit: u32,
    pub layer: Layer,
    pub point: LonLat,
    pub radius_m: f64,
}

/// How far a `place=*` node plausibly reaches, used when no boundary exists.
pub fn place_radius_m(value: &str) -> Option<f64> {
    Some(match value {
        "city" => 15_000.0,
        "town" => 6_000.0,
        "municipality" => 5_000.0,
        "village" => 2_500.0,
        "hamlet" => 1_000.0,
        "borough" => 3_000.0,
        "suburb" => 1_500.0,
        "quarter" => 1_000.0,
        "neighbourhood" | "city_block" => 500.0,
        _ => return None,
    })
}

const MAX_PLACE_RADIUS_M: f64 = 15_000.0;

pub struct LocalityIndex {
    points: Vec<LocalityPoint>,
    tree: Option<RTree<f64>>,
}

impl LocalityIndex {
    pub fn new(points: Vec<LocalityPoint>) -> Self {
        let tree = (!points.is_empty()).then(|| {
            let mut b = RTreeBuilder::<f64>::new(points.len() as u32);
            for p in &points {
                b.add(p.point.lon, p.point.lat, p.point.lon, p.point.lat);
            }
            b.finish::<HilbertSort>()
        });
        Self { points, tree }
    }

    /// The best place node of `layer` near `p`: smallest distance relative to
    /// the place's reach.
    pub fn best(&self, p: LonLat, layer: Layer) -> Option<u32> {
        let tree = self.tree.as_ref()?;
        let b = BBox::around(p, MAX_PLACE_RADIUS_M);
        tree.search(b.min_lon, b.min_lat, b.max_lon, b.max_lat)
            .into_iter()
            .map(|i| &self.points[i as usize])
            .filter(|l| l.layer == layer)
            .filter_map(|l| {
                let ratio = geom::haversine(p, l.point) / l.radius_m;
                (ratio <= 1.0).then_some((ratio, l.unit))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, unit)| unit)
    }
}
