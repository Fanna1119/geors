//! Streaming writer for the packed Hilbert R-tree in `spatial.idx`.
//!
//! The file is read with geo-index (flatbush layout, version 3, f32 boxes,
//! node size 16): an 8-byte header, the boxes of all nodes (leaves first,
//! then each level up to the root), then one index per node (a leaf's item
//! number, or an inner node's position of its first child). geo-index's
//! own builder keeps every box in RAM, plus its Hilbert sort (about 1.2 GB
//! for Germany). Here the leaves are sorted on disk and streamed straight
//! into the file; only the inner levels (1/16 of the leaves and up) are
//! held in memory.
//!
//! Items are sorted by Hilbert value, ties by item number. geo-index only
//! sorts to node-block granularity, so its files may differ in item order
//! within a node; both are valid trees with the same search results.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use geors_core::extsort::{ExternalSorter, buffer_bytes};

/// Flatbush format version and the f32 type code.
const VERSION: u8 = 3;
const F32_TYPE: u8 = 7;
pub const NODE_SIZE: usize = 16;

type Bbox = [f32; 4];

fn read_box(r: &mut impl Read) -> io::Result<Bbox> {
    let mut b = [0u8; 16];
    r.read_exact(&mut b)?;
    let v = |i: usize| f32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
    Ok([v(0), v(1), v(2), v(3)])
}

fn union(a: &mut Bbox, b: &Bbox) {
    a[0] = a[0].min(b[0]);
    a[1] = a[1].min(b[1]);
    a[2] = a[2].max(b[2]);
    a[3] = a[3].max(b[3]);
}

/// Total nodes and level bounds (in f32 elements), as geo-index computes
/// them.
fn num_nodes(num_items: usize) -> (usize, Vec<usize>) {
    let mut n = num_items;
    let mut total = n;
    let mut bounds = vec![n * 4];
    while n > 1 {
        n = n.div_ceil(NODE_SIZE);
        total += n;
        bounds.push(total * 4);
    }
    (total, bounds)
}

/// Write `spatial.idx` for the `count` boxes in `boxes_path` (16 bytes
/// each: min_x, min_y, max_x, max_y as little-endian f32, in item order).
/// Scratch files go to `dir`.
pub fn write(boxes_path: &Path, count: usize, out_path: &Path, dir: &Path) -> io::Result<()> {
    assert!(count >= 1, "an R-tree needs at least one item");
    let open = || -> io::Result<BufReader<File>> {
        Ok(BufReader::with_capacity(1 << 20, File::open(boxes_path)?))
    };
    // Pass 1: the extent, for scaling Hilbert coordinates.
    let mut extent: Bbox = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    let mut boxes = open()?;
    for _ in 0..count {
        union(&mut extent, &read_box(&mut boxes)?);
    }

    // Pass 2: sort `(hilbert << 32 | item, box)` on disk.
    let mut sorter = ExternalSorter::<[u64; 3]>::new(dir, "rtree", buffer_bytes(), false);
    let hilbert_max = ((1u32 << 16) - 1) as f64;
    let (width, height) = (
        (extent[2] - extent[0]) as f64,
        (extent[3] - extent[1]) as f64,
    );
    let mut boxes = open()?;
    for i in 0..count {
        let b = read_box(&mut boxes)?;
        // As geo-index: centre in f32, scaled to 0..=65535 (0 if the
        // extent is flat).
        let scale = |lo: f32, hi: f32, min: f32, size: f64| {
            let v = hilbert_max * (((lo + hi) as f64) / 2.0 - min as f64) / size;
            if v.is_finite() { v.floor() as u32 } else { 0 }
        };
        let h = hilbert(
            scale(b[0], b[2], extent[0], width),
            scale(b[1], b[3], extent[1], height),
        );
        let bits = |a: f32, c: f32| (u64::from(a.to_bits()) << 32) | u64::from(c.to_bits());
        sorter.push([
            (u64::from(h) << 32) | i as u64,
            bits(b[0], b[1]),
            bits(b[2], b[3]),
        ])?;
    }
    drop(boxes);
    let mut sorted = sorter.finish()?;

    // Pass 3: leaves straight to the file, their indices to a scratch file
    // (the indices follow all boxes), parents in memory.
    let (total, _) = num_nodes(count);
    let wide = total >= 16384; // u32 indices, else u16
    let mut out = BufWriter::with_capacity(1 << 20, File::create(out_path)?);
    out.write_all(&[0xfb, (VERSION << 4) + F32_TYPE])?;
    out.write_all(&(NODE_SIZE as u16).to_le_bytes())?;
    out.write_all(&(count as u32).to_le_bytes())?;
    let leaf_index_path = dir.join("rtree-leaf-indices.tmp");
    let mut leaf_indices = BufWriter::with_capacity(1 << 20, File::create(&leaf_index_path)?);
    let write_index = |w: &mut BufWriter<File>, v: usize| -> io::Result<()> {
        if wide {
            w.write_all(&(v as u32).to_le_bytes())
        } else {
            w.write_all(&(v as u16).to_le_bytes())
        }
    };
    // (box, position of its first child in f32 elements) per inner node.
    let mut level: Vec<(Bbox, usize)> = Vec::with_capacity(count.div_ceil(NODE_SIZE));
    let mut node: Bbox = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for leaf in 0..count {
        let [key, a, c] = sorted
            .next_record()?
            .ok_or_else(|| io::Error::other("R-tree sort lost items"))?;
        let unbits = |v: u64| (f32::from_bits((v >> 32) as u32), f32::from_bits(v as u32));
        let ((min_x, min_y), (max_x, max_y)) = (unbits(a), unbits(c));
        let b = [min_x, min_y, max_x, max_y];
        for v in b {
            out.write_all(&v.to_le_bytes())?;
        }
        write_index(&mut leaf_indices, (key & 0xFFFF_FFFF) as usize)?;
        if leaf % NODE_SIZE == 0 {
            node = b;
        } else {
            union(&mut node, &b);
        }
        if leaf % NODE_SIZE == NODE_SIZE - 1 || leaf + 1 == count {
            level.push((node, (leaf - leaf % NODE_SIZE) * 4));
        }
    }
    drop(sorted);
    leaf_indices.flush()?;
    drop(leaf_indices);

    // Inner levels, bottom up: each groups NODE_SIZE consecutive nodes of
    // the level below. A single item has no inner level.
    let mut inner: Vec<(Bbox, usize)> = Vec::new();
    if count > 1 {
        let mut start = count; // node number of the level's first node
        loop {
            let len = level.len();
            inner.extend_from_slice(&level);
            if len == 1 {
                break;
            }
            let next: Vec<(Bbox, usize)> = level
                .chunks(NODE_SIZE)
                .enumerate()
                .map(|(k, group)| {
                    let mut b = group[0].0;
                    for (g, _) in &group[1..] {
                        union(&mut b, g);
                    }
                    (b, (start + k * NODE_SIZE) * 4)
                })
                .collect();
            start += len;
            level = next;
        }
    }
    debug_assert_eq!(count + inner.len(), total);
    for (b, _) in &inner {
        for v in b {
            out.write_all(&v.to_le_bytes())?;
        }
    }
    io::copy(&mut File::open(&leaf_index_path)?, &mut out)?;
    std::fs::remove_file(&leaf_index_path)?;
    for (_, first_child) in &inner {
        write_index(&mut out, *first_child)?;
    }
    out.flush()
}

/// Hilbert curve position of `(x, y)` in a 2^16 grid (as geo-index /
/// flatbush compute it).
fn hilbert(x: u32, y: u32) -> u32 {
    let mut a_1 = x ^ y;
    let mut b_1 = 0xFFFF ^ a_1;
    let mut c_1 = 0xFFFF ^ (x | y);
    let mut d_1 = x & (y ^ 0xFFFF);

    let mut a_2 = a_1 | (b_1 >> 1);
    let mut b_2 = (a_1 >> 1) ^ a_1;
    let mut c_2 = ((c_1 >> 1) ^ (b_1 & (d_1 >> 1))) ^ c_1;
    let mut d_2 = ((a_1 & (c_1 >> 1)) ^ (d_1 >> 1)) ^ d_1;

    a_1 = a_2;
    b_1 = b_2;
    c_1 = c_2;
    d_1 = d_2;
    a_2 = (a_1 & (a_1 >> 2)) ^ (b_1 & (b_1 >> 2));
    b_2 = (a_1 & (b_1 >> 2)) ^ (b_1 & ((a_1 ^ b_1) >> 2));
    c_2 ^= (a_1 & (c_1 >> 2)) ^ (b_1 & (d_1 >> 2));
    d_2 ^= (b_1 & (c_1 >> 2)) ^ ((a_1 ^ b_1) & (d_1 >> 2));

    a_1 = a_2;
    b_1 = b_2;
    c_1 = c_2;
    d_1 = d_2;
    a_2 = (a_1 & (a_1 >> 4)) ^ (b_1 & (b_1 >> 4));
    b_2 = (a_1 & (b_1 >> 4)) ^ (b_1 & ((a_1 ^ b_1) >> 4));
    c_2 ^= (a_1 & (c_1 >> 4)) ^ (b_1 & (d_1 >> 4));
    d_2 ^= (b_1 & (c_1 >> 4)) ^ ((a_1 ^ b_1) & (d_1 >> 4));

    a_1 = a_2;
    b_1 = b_2;
    c_1 = c_2;
    d_1 = d_2;
    c_2 ^= (a_1 & (c_1 >> 8)) ^ (b_1 & (d_1 >> 8));
    d_2 ^= (b_1 & (c_1 >> 8)) ^ ((a_1 ^ b_1) & (d_1 >> 8));

    a_1 = c_2 ^ (c_2 >> 1);
    b_1 = d_2 ^ (d_2 >> 1);

    let mut i0 = x ^ y;
    let mut i1 = b_1 | (0xFFFF ^ (i0 | a_1));

    i0 = (i0 | (i0 << 8)) & 0x00FF_00FF;
    i0 = (i0 | (i0 << 4)) & 0x0F0F_0F0F;
    i0 = (i0 | (i0 << 2)) & 0x3333_3333;
    i0 = (i0 | (i0 << 1)) & 0x5555_5555;

    i1 = (i1 | (i1 << 8)) & 0x00FF_00FF;
    i1 = (i1 | (i1 << 4)) & 0x0F0F_0F0F;
    i1 = (i1 | (i1 << 2)) & 0x3333_3333;
    i1 = (i1 | (i1 << 1)) & 0x5555_5555;

    (i1 << 1) | i0
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_index::rtree::sort::HilbertSort;
    use geo_index::rtree::{RTreeBuilder, RTreeIndex, RTreeRef};

    fn rng(seed: &mut u64) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed % 1_000_000) as f32 / 1_000_000.0
    }

    /// Same search results as geo-index's own builder, for every size
    /// class: one item, one node, two levels, and u32 indices.
    #[test]
    fn matches_geo_index() {
        for count in [1usize, 2, 16, 17, 255, 5_000, 20_000] {
            let dir = tempfile::tempdir().unwrap();
            let mut seed = 7 + count as u64;
            let mut boxes = Vec::new();
            for i in 0..count {
                let (x, y) = (rng(&mut seed) * 10.0, 40.0 + rng(&mut seed) * 10.0);
                // Points and boxes, some identical (ties).
                let (w, h) = if i % 3 == 0 {
                    (0.0, 0.0)
                } else {
                    (rng(&mut seed) * 0.1, 0.05)
                };
                let b = if i % 50 == 1 {
                    [1.0, 41.0, 1.0, 41.0]
                } else {
                    [x, y, x + w, y + h]
                };
                boxes.push(b);
            }
            let boxes_path = dir.path().join("boxes");
            let mut f = File::create(&boxes_path).unwrap();
            for b in &boxes {
                for v in b {
                    f.write_all(&v.to_le_bytes()).unwrap();
                }
            }
            drop(f);
            let out = dir.path().join("spatial.idx");
            write(&boxes_path, count, &out, dir.path()).unwrap();
            let ours = std::fs::read(&out).unwrap();
            let ours = RTreeRef::<f32>::try_new(&ours).unwrap();

            let mut builder = RTreeBuilder::<f32>::new(count as u32);
            for b in &boxes {
                builder.add(b[0], b[1], b[2], b[3]);
            }
            let theirs = builder.finish::<HilbertSort>();
            assert_eq!(ours.num_items(), theirs.num_items(), "{count} items");
            for _ in 0..200 {
                let (x, y) = (rng(&mut seed) * 10.0, 40.0 + rng(&mut seed) * 10.0);
                let s = rng(&mut seed) * 2.0;
                let mut a = ours.search(x, y, x + s, y + s);
                let mut b = theirs.search(x, y, x + s, y + s);
                a.sort_unstable();
                b.sort_unstable();
                assert_eq!(a, b, "{count} items, query at {x},{y}");
            }
            // Only the input and the output are left behind.
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        }
    }
}
