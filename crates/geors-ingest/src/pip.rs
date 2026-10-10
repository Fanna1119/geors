//! Fast point-in-polygon for large boundaries.
//!
//! A boundary's edges are bucketed into horizontal bands. A point test only
//! looks at the edges in the point's band (ray casting, even-odd over all
//! rings, so holes work), instead of every vertex of, say, a country
//! border. Points exactly on an edge count as inside.

use geo::kernels::{Kernel, Orientation, RobustKernel};
use geo::{Coord, MultiPolygon};
use geors_core::geom::{BBox, from_e7, to_e7};

/// Average number of edges per band to aim for.
const EDGES_PER_BAND: usize = 8;
const MAX_BANDS: usize = 4096;

pub struct BandedPolygon {
    /// All ring vertices, fixed point (1e-7 degrees, as in OSM): half the
    /// memory of f64 and exact for OSM data. Edge `i` runs from vertex `i`
    /// to `i + 1`; only edges within a ring are indexed.
    pts: Vec<[i32; 2]>,
    bands: Bands,
    /// CSR layout: edges of band `b` are `edges[offsets[b]..offsets[b + 1]]`.
    offsets: Vec<u32>,
    edges: Vec<u32>,
    pub bbox: BBox,
}

/// Band layout of a polygon.
#[derive(Clone, Copy)]
struct Bands {
    min_y: f64,
    height: f64,
    n: usize,
}

impl Bands {
    fn band(&self, y: f64) -> usize {
        let b = ((y - self.min_y) / self.height).floor();
        (b.max(0.0) as usize).min(self.n.saturating_sub(1))
    }

    fn range(&self, y0: f64, y1: f64) -> (usize, usize) {
        (self.band(y0), self.band(y1))
    }
}

impl BandedPolygon {
    pub fn new(mp: &MultiPolygon<f64>) -> Self {
        let mut pts = Vec::new();
        // Whether vertex i starts an edge (false at the end of a ring);
        // only needed while building.
        let mut has_edge = Vec::new();
        let mut bbox = BBox::empty();
        for poly in mp {
            for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
                let n = ring.0.len();
                for (i, c) in ring.0.iter().enumerate() {
                    pts.push([to_e7(c.x), to_e7(c.y)]);
                    has_edge.push(i + 1 < n);
                    bbox.extend(geors_core::LonLat::new(c.x, c.y));
                }
            }
        }
        let edge_ids: Vec<usize> = (0..has_edge.len()).filter(|&i| has_edge[i]).collect();
        drop(has_edge);
        let n = (edge_ids.len() / EDGES_PER_BAND).clamp(1, MAX_BANDS);
        let bands = Bands {
            min_y: bbox.min_lat,
            height: ((bbox.max_lat - bbox.min_lat) / n as f64).max(f64::MIN_POSITIVE),
            n,
        };
        let y = |i: usize| from_e7(pts[i][1]);
        // Two passes (count, fill) to build the CSR arrays.
        let mut counts = vec![0u32; n + 1];
        for &i in &edge_ids {
            let (lo, hi) = bands.range(y(i).min(y(i + 1)), y(i).max(y(i + 1)));
            for b in lo..=hi {
                counts[b + 1] += 1;
            }
        }
        for b in 0..n {
            counts[b + 1] += counts[b];
        }
        let mut fill = counts.clone();
        let mut edges = vec![0u32; counts[n] as usize];
        for &i in &edge_ids {
            let (lo, hi) = bands.range(y(i).min(y(i + 1)), y(i).max(y(i + 1)));
            for b in lo..=hi {
                edges[fill[b] as usize] = i as u32;
                fill[b] += 1;
            }
        }
        pts.shrink_to_fit();
        Self {
            pts,
            bands,
            offsets: counts,
            edges,
            bbox,
        }
    }

    fn view(&self) -> PolygonView<'_> {
        PolygonView {
            pts: &self.pts,
            offsets: &self.offsets,
            edges: &self.edges,
            bands: self.bands,
            bbox: self.bbox,
        }
    }

    #[cfg(test)]
    fn bands(&self) -> usize {
        self.bands.n
    }

    /// Point inside or on the boundary.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.view().contains(x, y)
    }

    /// Whether any edge passes through the box.
    pub fn crosses(&self, b: &BBox) -> bool {
        self.view().crosses(b)
    }
}

/// A polygon's arrays, wherever they are stored (heap or [`PolygonStore`]).
struct PolygonView<'a> {
    pts: &'a [[i32; 2]],
    offsets: &'a [u32],
    edges: &'a [u32],
    bands: Bands,
    bbox: BBox,
}

impl PolygonView<'_> {
    fn band_edges(&self, b: usize) -> &[u32] {
        &self.edges[self.offsets[b] as usize..self.offsets[b + 1] as usize]
    }

    fn edge(&self, i: u32) -> (f64, f64, f64, f64) {
        let (a, b) = (self.pts[i as usize], self.pts[i as usize + 1]);
        (from_e7(a[0]), from_e7(a[1]), from_e7(b[0]), from_e7(b[1]))
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        if !self.bbox.contains(geors_core::LonLat::new(x, y)) || self.bands.n == 0 {
            return false;
        }
        let p = Coord { x, y };
        let mut inside = false;
        for &e in self.band_edges(self.bands.band(y)) {
            let (x1, y1, x2, y2) = self.edge(e);
            let (a, b) = (Coord { x: x1, y: y1 }, Coord { x: x2, y: y2 });
            // Robust orientation: exact, no rounding near edges.
            let o = RobustKernel::orient2d(a, b, p);
            // On the edge: collinear and within the segment's box.
            if o == Orientation::Collinear
                && x >= x1.min(x2)
                && x <= x1.max(x2)
                && y >= y1.min(y2)
                && y <= y1.max(y2)
            {
                return true;
            }
            // Ray towards +x crosses an edge that straddles y and has the
            // point on its left (upward edge) or right (downward edge).
            if (y1 > y) != (y2 > y) {
                let left = o == Orientation::CounterClockwise;
                if left == (y2 > y1) {
                    inside = !inside;
                }
            }
        }
        inside
    }

    fn crosses(&self, b: &BBox) -> bool {
        if !self.bbox.intersects(b) || self.bands.n == 0 {
            return false;
        }
        let (lo, hi) = self.bands.range(b.min_lat, b.max_lat);
        (lo..=hi).any(|band| {
            self.band_edges(band).iter().any(|&e| {
                let (x1, y1, x2, y2) = self.edge(e);
                segment_hits_box(x1, y1, x2, y2, b)
            })
        })
    }
}

/// Where a polygon's arrays are in a [`PolygonStore`]: 4-byte word offsets
/// and lengths. About 80 bytes per polygon stay on the heap.
#[derive(Clone, Copy)]
pub struct StoredPolygon {
    pts: (u64, u32),
    offsets: (u64, u32),
    edges: (u64, u32),
    bands: Bands,
    pub bbox: BBox,
}

/// Polygon arrays in a memory-mapped scratch file.
///
/// Boundary polygons are the largest structure an import keeps through
/// passes 4 and 5 (Germany: 270 MB, Europe: gigabytes). In a mapped file
/// they live in the page cache instead of the heap: shared, and dropped by
/// the OS under memory pressure, then read back on demand.
pub struct PolygonStore {
    writer: Option<std::io::BufWriter<std::fs::File>>,
    words: u64,
    map: Option<memmap2::Mmap>,
    path: std::path::PathBuf,
}

impl PolygonStore {
    pub fn create(path: &std::path::Path) -> std::io::Result<Self> {
        Ok(Self {
            // Readable too: the file is mapped once written.
            writer: Some(std::io::BufWriter::with_capacity(
                1 << 20,
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)?,
            )),
            words: 0,
            map: None,
            path: path.to_path_buf(),
        })
    }

    fn write(&mut self, data: &[u8]) -> std::io::Result<(u64, u32)> {
        use std::io::Write;
        let w = self.writer.as_mut().expect("store already finished");
        w.write_all(data)?;
        let at = self.words;
        let len = (data.len() / 4) as u32;
        self.words += len as u64;
        Ok((at, len))
    }

    /// Move a polygon's arrays to the store.
    pub fn push(&mut self, p: BandedPolygon) -> std::io::Result<StoredPolygon> {
        Ok(StoredPolygon {
            pts: self.write(bytemuck::cast_slice(&p.pts))?,
            offsets: self.write(bytemuck::cast_slice(&p.offsets))?,
            edges: self.write(bytemuck::cast_slice(&p.edges))?,
            bands: p.bands,
            bbox: p.bbox,
        })
    }

    /// Stop writing and map the file. Written pages are flushed first: in a
    /// container they count against the memory limit until they reach the
    /// disk.
    pub fn finish(&mut self) -> std::io::Result<()> {
        if let Some(w) = self.writer.take() {
            let file = w.into_inner().map_err(|e| e.into_error())?;
            file.sync_data()?;
            if self.words > 0 {
                // SAFETY: scratch file private to this import, not modified
                // after this point.
                self.map = Some(unsafe { memmap2::Mmap::map(&file)? });
            }
        }
        Ok(())
    }

    fn slice<T: bytemuck::Pod>(&self, (at, len): (u64, u32)) -> &[T] {
        let bytes = self.map.as_deref().unwrap_or_default();
        bytemuck::cast_slice(&bytes[at as usize * 4..(at as usize + len as usize) * 4])
    }

    fn view(&self, p: &StoredPolygon) -> PolygonView<'_> {
        PolygonView {
            pts: self.slice(p.pts),
            offsets: self.slice(p.offsets),
            edges: self.slice(p.edges),
            bands: p.bands,
            bbox: p.bbox,
        }
    }

    /// Point inside or on the boundary of `p`.
    pub fn contains(&self, p: &StoredPolygon, x: f64, y: f64) -> bool {
        self.view(p).contains(x, y)
    }

    /// Whether any edge of `p` passes through the box.
    pub fn crosses(&self, p: &StoredPolygon, b: &BBox) -> bool {
        self.view(p).crosses(b)
    }

    /// Bytes in the store (for memory diagnostics).
    pub fn bytes(&self) -> u64 {
        self.words * 4
    }
}

impl StoredPolygon {
    /// Number of vertices.
    pub fn vertices(&self) -> usize {
        self.pts.1 as usize / 2
    }
}

impl Drop for PolygonStore {
    fn drop(&mut self) {
        self.map = None;
        self.writer = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Liang–Barsky clipping: does segment (x1,y1)-(x2,y2) touch the box?
fn segment_hits_box(x1: f64, y1: f64, x2: f64, y2: f64, b: &BBox) -> bool {
    let (dx, dy) = (x2 - x1, y2 - y1);
    let mut t0: f64 = 0.0;
    let mut t1: f64 = 1.0;
    for (p, q) in [
        (-dx, x1 - b.min_lon),
        (dx, b.max_lon - x1),
        (-dy, y1 - b.min_lat),
        (dy, b.max_lat - y1),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
            if t0 > t1 {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Contains, LineString, Polygon};

    fn ring(pts: &[(f64, f64)]) -> LineString<f64> {
        LineString::new(pts.iter().map(|&(x, y)| Coord { x, y }).collect())
    }

    /// Square with a square hole, plus a separate island.
    fn shape() -> MultiPolygon<f64> {
        MultiPolygon::new(vec![
            Polygon::new(
                ring(&[
                    (0.0, 0.0),
                    (10.0, 0.0),
                    (10.0, 10.0),
                    (0.0, 10.0),
                    (0.0, 0.0),
                ]),
                vec![ring(&[
                    (4.0, 4.0),
                    (6.0, 4.0),
                    (6.0, 6.0),
                    (4.0, 6.0),
                    (4.0, 4.0),
                ])],
            ),
            Polygon::new(
                ring(&[(20.0, 0.0), (22.0, 1.0), (21.0, 3.0), (20.0, 0.0)]),
                vec![],
            ),
        ])
    }

    #[test]
    fn matches_geo_contains() {
        let mp = shape();
        let p = BandedPolygon::new(&mp);
        let mut seed = 1u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let x = (seed % 25_000) as f64 / 1000.0 - 1.0;
            let y = ((seed >> 20) % 12_000) as f64 / 1000.0 - 1.0;
            let c = Coord { x, y };
            // Off-boundary points must agree with geo exactly.
            if !geo::Intersects::intersects(
                &mp.iter()
                    .flat_map(|p| {
                        std::iter::once(p.exterior().clone()).chain(p.interiors().iter().cloned())
                    })
                    .collect::<geo::MultiLineString<f64>>(),
                &c,
            ) {
                assert_eq!(p.contains(x, y), mp.contains(&c), "{x},{y}");
            }
        }
    }

    /// A many-vertex ring (so many bands are used): a jagged circle.
    #[test]
    fn matches_geo_with_many_bands() {
        let n = 5_000;
        let pts: Vec<(f64, f64)> = (0..=n)
            .map(|i| {
                let t = i as f64 / n as f64 * std::f64::consts::TAU;
                let r = 1.0 + 0.05 * ((i % 7) as f64);
                (r * t.cos(), r * t.sin())
            })
            .collect();
        let mp = MultiPolygon::new(vec![Polygon::new(ring(&pts), vec![])]);
        let p = BandedPolygon::new(&mp);
        assert!(p.bands() > 100);
        let mut seed = 3u64;
        for _ in 0..5_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let x = (seed % 30_000) as f64 / 10_000.0 - 1.5;
            let y = ((seed >> 20) % 30_000) as f64 / 10_000.0 - 1.5;
            assert_eq!(p.contains(x, y), mp.contains(&Coord { x, y }), "{x},{y}");
        }
        assert!(p.crosses(&BBox::new(0.99, -0.01, 1.01, 0.01)));
        assert!(!p.crosses(&BBox::new(-0.1, -0.1, 0.1, 0.1)));
    }

    #[test]
    fn boundary_is_inside() {
        let p = BandedPolygon::new(&shape());
        assert!(p.contains(0.0, 5.0)); // left edge
        assert!(p.contains(10.0, 10.0)); // corner
        assert!(p.contains(5.0, 4.0)); // hole edge
        assert!(!p.contains(5.0, 5.0)); // inside hole
        assert!(p.contains(21.0, 1.0)); // island
        assert!(!p.contains(15.0, 5.0));
    }

    #[test]
    fn stored_polygons_match_heap_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PolygonStore::create(&dir.path().join("polys")).unwrap();
        let shapes = [shape(), shape()];
        let stored: Vec<StoredPolygon> = shapes
            .iter()
            .map(|mp| store.push(BandedPolygon::new(mp)).unwrap())
            .collect();
        store.finish().unwrap();
        let heap = BandedPolygon::new(&shape());
        for sp in &stored {
            for (x, y) in [
                (0.0, 5.0),
                (5.0, 5.0),
                (5.0, 4.0),
                (21.0, 1.0),
                (15.0, 5.0),
                (3.0, 3.0),
            ] {
                assert_eq!(store.contains(sp, x, y), heap.contains(x, y), "{x},{y}");
            }
            let b = BBox::new(-1.0, 4.0, 1.0, 5.0);
            assert_eq!(store.crosses(sp, &b), heap.crosses(&b));
        }
        let path = dir.path().join("polys");
        drop(store);
        assert!(!path.exists());
    }

    #[test]
    fn crossing_boxes() {
        let p = BandedPolygon::new(&shape());
        assert!(p.crosses(&BBox::new(-1.0, 4.0, 1.0, 5.0))); // over left edge
        assert!(p.crosses(&BBox::new(4.5, 3.5, 5.5, 4.5))); // over hole edge
        assert!(!p.crosses(&BBox::new(1.0, 1.0, 3.0, 3.0))); // fully inside
        assert!(!p.crosses(&BBox::new(4.5, 4.5, 5.5, 5.5))); // fully in hole
        assert!(!p.crosses(&BBox::new(12.0, 1.0, 13.0, 2.0))); // outside
    }
}
