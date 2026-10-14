//! Writes a country partition to disk (see `geors_core::storage` for the layout).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;

use geors_core::doc::{self, TagTable};
use geors_core::extsort::{ExternalSorter, Sorted, buffer_bytes};
use geors_core::geom::{f32_down, f32_up};
use geors_core::spill::{Spill, SpillEntry, SpillReader, SpillWriter, WorkDir, decode_record};
use geors_core::storage::{self, FORMAT_VERSION, GeomKind, PartitionMeta, PlaceRecord};
use geors_core::{AdminUnit, BBox, Place};
use tracing::{info, warn};

use crate::IndexError;
use crate::text::TextIndexWriter;

pub struct PartitionInput {
    pub country_code: String,
    pub country_name: Option<String>,
    pub source: String,
    /// The partition's places, spilled to disk during import.
    pub places: Spill,
    /// Admin table that `Place::parents` index into. May be shared by
    /// several partitions; each partition stores only the units it uses.
    pub admins: Arc<Vec<AdminUnit>>,
}

/// Interleave the bits of two 16 bit values: a cheap Z-order key that puts
/// nearby places close together on disk, which helps the page cache.
fn morton(lon: f64, lat: f64) -> u32 {
    let x = (((lon + 180.0) / 360.0) * 65535.0) as u32;
    let y = (((lat + 90.0) / 180.0) * 65535.0) as u32;
    let spread = |mut v: u32| {
        v &= 0xFFFF;
        v = (v | (v << 8)) & 0x00FF00FF;
        v = (v | (v << 4)) & 0x0F0F0F0F;
        v = (v | (v << 2)) & 0x33333333;
        (v | (v << 1)) & 0x55555555
    };
    spread(x) | (spread(y) << 1)
}

/// Sort key of a place: `[morton << 8 | osm type, osm id (order-preserving
/// as unsigned), spill offset, record length]`. Spatial order; ties broken
/// by OSM id, then by file position, so the output is deterministic
/// whatever order the import produced places in.
type Key = [u64; 4];

fn key(e: &SpillEntry) -> Key {
    [
        (u64::from(morton(e.center.lon, e.center.lat)) << 8) | u64::from(e.osm_type),
        (e.osm_id as u64) ^ (1 << 63),
        e.offset,
        u64::from(e.len),
    ]
}

/// Places decoded per parallel chunk.
const CHUNK: usize = 8192;

#[derive(Clone, Copy)]
struct WriterMemory {
    /// Spill files up to this size are read in place, in spatial order.
    direct_max: u64,
    /// RAM for one bucket of a larger spill file.
    bucket_bytes: usize,
}

static MEMORY: std::sync::Mutex<WriterMemory> = std::sync::Mutex::new(WriterMemory {
    direct_max: u64::MAX,
    bucket_bytes: 256 << 20,
});

fn memory() -> WriterMemory {
    *MEMORY.lock().unwrap()
}

/// Memory for reordering places while writing. Places are written in
/// spatial order, which is a random order in the spill file. A spill file
/// up to `direct_max` bytes is read in place: it should fit in the page
/// cache. A larger one is first split, in one sequential pass, into
/// buckets of consecutive spatial ranges of at most `bucket_bytes` each;
/// each bucket is then read into RAM whole. Without this, a spill file
/// much larger than the page cache costs a disk read per place.
pub fn set_writer_memory(direct_max: u64, bucket_bytes: usize) {
    *MEMORY.lock().unwrap() = WriterMemory {
        direct_max,
        bucket_bytes: bucket_bytes.max(1 << 20),
    };
}

/// Reads the places of a spill file in sort order. The sort keys are sorted
/// externally (sorted runs on disk, merged sequentially), so no per-place
/// data stays in RAM: 32 bytes per place would be 790 MB for Germany.
enum Ordered {
    /// Sorted keys streamed; records read from the spill file in place.
    Direct { keys: Sorted<Key> },
    /// The records regrouped into buckets of consecutive sort ranges.
    Buckets {
        file: fs::File,
        path: PathBuf,
        /// The sorted keys, written once while assigning buckets.
        keys: std::io::BufReader<fs::File>,
        keys_path: PathBuf,
        /// Per bucket: number of places, byte range in `file`.
        buckets: Vec<(usize, std::ops::Range<u64>)>,
    },
}

impl Ordered {
    /// Sort the spill file's keys. With `direct`, records are later read in
    /// place; otherwise they are regrouped into buckets of at most
    /// `bucket_bytes` (see [`set_writer_memory`]). Scratch files go to `dir`.
    fn open(
        spill: &SpillReader,
        direct: bool,
        bucket_bytes: usize,
        dir: &Path,
    ) -> std::io::Result<Self> {
        let mut sorter = ExternalSorter::<Key>::new(dir, "sort-keys", buffer_bytes(), false);
        let mut result = Ok(());
        spill.advise_sequential();
        spill.for_each_entry(|e| {
            if result.is_ok() {
                result = sorter.push(key(&e));
            }
        })?;
        result?;
        let keys = sorter.finish()?;
        if direct {
            Ok(Self::Direct { keys })
        } else {
            Self::bucketed(spill, keys, bucket_bytes, dir)
        }
    }

    fn bucketed(
        spill: &SpillReader,
        mut sorted: Sorted<Key>,
        bucket_bytes: usize,
        dir: &Path,
    ) -> std::io::Result<Self> {
        use std::io::{BufReader, Write};
        use std::os::unix::fs::FileExt;
        // Pass 1 over the sorted keys: cut them into buckets of at most
        // `bucket_bytes`, keep the keys in a file for reading the buckets
        // back, and note each record's bucket by spill offset.
        let keys_path = dir.join("sort-keys.tmp");
        let mut keys_out = BufWriter::with_capacity(1 << 20, fs::File::create(&keys_path)?);
        let mut route = ExternalSorter::<[u64; 2]>::new(dir, "route", buffer_bytes(), false);
        let mut sizes: Vec<(usize, u64)> = Vec::new();
        let (mut n, mut bytes) = (0usize, 0u64);
        while let Some(k) = sorted.next_record()? {
            let len = k[3];
            if n > 0 && bytes + len > bucket_bytes as u64 {
                sizes.push((n, bytes));
                (n, bytes) = (0, 0);
            }
            keys_out.write_all(bytemuck::bytes_of(&k))?;
            route.push([k[2], sizes.len() as u64])?;
            n += 1;
            bytes += len;
        }
        if n > 0 {
            sizes.push((n, bytes));
        }
        drop(sorted);
        keys_out.flush()?;
        drop(keys_out);
        let mut at = 0u64;
        let buckets: Vec<(usize, std::ops::Range<u64>)> = sizes
            .into_iter()
            .map(|(n, b)| {
                at += b;
                (n, at - b..at)
            })
            .collect();

        // Pass 2, sequential over the spill file: append each record to its
        // bucket's region, through a small buffer per bucket (all buffers:
        // `bucket_bytes`). The route is sorted by offset, as the file is.
        let path = dir.join("reorder.tmp");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        let mut route = route.finish()?;
        let nb = buckets.len().max(1);
        let buf_size = (bucket_bytes / nb).clamp(4 << 10, 1 << 20);
        let mut cursors: Vec<u64> = buckets.iter().map(|(_, r)| r.start).collect();
        let mut bufs: Vec<Vec<u8>> = vec![Vec::new(); buckets.len()];
        let mut result = Ok(());
        spill.for_each_entry(|e| {
            if result.is_err() {
                return;
            }
            let b = match route.next_record() {
                Ok(Some([offset, b])) if offset == e.offset => b as usize,
                Ok(_) => {
                    result = Err(std::io::Error::other("spill file and sort keys disagree"));
                    return;
                }
                Err(err) => {
                    result = Err(err);
                    return;
                }
            };
            let buf = &mut bufs[b];
            buf.extend_from_slice(spill.record(&e));
            if buf.len() >= buf_size {
                result = file.write_all_at(buf, cursors[b]);
                cursors[b] += buf.len() as u64;
                buf.clear();
            }
        })?;
        result?;
        drop(route);
        for (b, buf) in bufs.iter().enumerate() {
            file.write_all_at(buf, cursors[b])?;
        }
        // Written pages count against a container's memory limit until
        // they reach the disk; flush them so they can be reclaimed.
        file.sync_data()?;
        info!(
            buckets = buckets.len(),
            bucket_mb = bucket_bytes >> 20,
            "spill file regrouped for sequential reads"
        );
        Ok(Self::Buckets {
            file,
            path,
            keys: BufReader::with_capacity(1 << 20, fs::File::open(&keys_path)?),
            keys_path,
            buckets,
        })
    }

    /// Decode all places in sort order, handing them to `f` in chunks.
    fn for_each_chunk(
        &mut self,
        spill: &SpillReader,
        mut f: impl FnMut(Vec<Place>) -> Result<(), IndexError>,
    ) -> Result<(), IndexError> {
        match self {
            Self::Direct { keys } => {
                let mut chunk: Vec<Key> = Vec::with_capacity(CHUNK);
                loop {
                    chunk.clear();
                    while chunk.len() < CHUNK {
                        match keys.next_record()? {
                            Some(k) => chunk.push(k),
                            None => break,
                        }
                    }
                    if chunk.is_empty() {
                        break;
                    }
                    f(chunk
                        .par_iter()
                        .map(|k| spill.get(k[2]))
                        .collect::<std::io::Result<_>>()?)?;
                }
            }
            Self::Buckets {
                file,
                keys,
                buckets,
                ..
            } => {
                use std::io::Read;
                use std::os::unix::fs::FileExt;
                let mut data = Vec::new();
                for (n, bytes) in buckets.iter() {
                    let mut bucket_keys = vec![[0u64; 4]; *n];
                    keys.read_exact(bytemuck::cast_slice_mut(&mut bucket_keys))?;
                    data.resize((bytes.end - bytes.start) as usize, 0);
                    file.read_exact_at(&mut data, bytes.start)?;
                    // Records are in file order within the bucket: walk
                    // them by spill offset to find each one's position.
                    let mut by_offset: Vec<u32> = (0..*n as u32).collect();
                    by_offset.sort_unstable_by_key(|&k| bucket_keys[k as usize][2]);
                    let mut pos = vec![0u32; *n];
                    let mut at = 0usize;
                    for k in by_offset {
                        pos[k as usize] = at as u32;
                        let len = data
                            .get(at..at + 4)
                            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                            .ok_or_else(|| IndexError::Format("truncated bucket".into()))?;
                        at += 4 + len as usize;
                    }
                    for chunk in pos.chunks(CHUNK) {
                        f(chunk
                            .par_iter()
                            .map(|&p| decode_record(&data, p as usize))
                            .collect::<std::io::Result<_>>()?)?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Drop for Ordered {
    fn drop(&mut self) {
        if let Self::Buckets {
            path, keys_path, ..
        } = self
        {
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(keys_path);
        }
    }
}

/// Convenience for small inputs (tests, tools): spill `places` to a scratch
/// file and write the partition.
pub fn write_partition_from_places(
    data_dir: &Path,
    country_code: &str,
    country_name: Option<String>,
    source: &str,
    places: &[Place],
    admins: Vec<AdminUnit>,
) -> Result<PathBuf, IndexError> {
    fs::create_dir_all(data_dir)?;
    let work = WorkDir::create(data_dir, &format!("{country_code}.spill"))?;
    let mut w = SpillWriter::create(&work.path.join("places.spill"))?;
    for p in places {
        w.push(p)?;
    }
    write_partition(
        data_dir,
        PartitionInput {
            country_code: country_code.to_string(),
            country_name,
            source: source.to_string(),
            places: w.finish()?,
            admins: Arc::new(admins),
        },
    )
}

/// Build all partition files in a temporary directory, then atomically swap
/// it into `<data_dir>/<cc>`.
///
/// Memory use is independent of the partition size, apart from the R-tree
/// builder (built last, after the text index): sort keys are sorted on
/// disk, and places are streamed from the spill file in spatial (Z-order)
/// order.
pub fn write_partition(data_dir: &Path, input: PartitionInput) -> Result<PathBuf, IndexError> {
    let cc = input.country_code.clone();
    let count = input.places.count;
    if count == 0 {
        return Err(IndexError::Format(format!(
            "partition '{cc}' has no places"
        )));
    }
    if count > u32::MAX as u64 {
        return Err(IndexError::Format(format!(
            "partition '{cc}' has too many places"
        )));
    }
    let t = Instant::now();
    fs::create_dir_all(data_dir)?;
    let final_dir = storage::partition_dir(data_dir, &cc);
    let tmp_dir = data_dir.join(format!(".{cc}.tmp-{}", std::process::id()));
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir)?;
    }
    fs::create_dir_all(&tmp_dir)?;
    geors_core::mem::mark("writer: start");

    let spill = SpillReader::open(&input.places)?;
    let file_len = fs::metadata(&input.places.path)?.len();
    let mem = memory();
    let mut reader = Ordered::open(
        &spill,
        file_len <= mem.direct_max,
        mem.bucket_bytes,
        &tmp_dir,
    )?;
    geors_core::mem::mark("writer: reader ready");

    let mut docs =
        BufWriter::with_capacity(1 << 20, fs::File::create(tmp_dir.join(storage::DOCS_FILE))?);
    let mut places_out = BufWriter::with_capacity(
        1 << 20,
        fs::File::create(tmp_dir.join(storage::PLACES_FILE))?,
    );
    let mut importance = BufWriter::with_capacity(
        1 << 20,
        fs::File::create(tmp_dir.join(storage::IMPORTANCE_FILE))?,
    );
    let mut geom =
        BufWriter::with_capacity(1 << 20, fs::File::create(tmp_dir.join(storage::GEOM_FILE))?);
    let mut text = TextIndexWriter::create(&tmp_dir.join(storage::TEXT_DIR))?;
    // f32 boxes, rounded outward: half the memory and disk of f64, and a
    // query can only get extra candidates, never miss one. They go to a
    // scratch file first: the R-tree is built after the text index, so the
    // two largest allocations of this function do not overlap.
    let boxes_path = tmp_dir.join("boxes.tmp");
    let mut boxes = BufWriter::with_capacity(1 << 20, fs::File::create(&boxes_path)?);
    geors_core::mem::mark("writer: text writer created");

    let mut doc_offset = 0u64;
    let mut geom_offset = 0u64;
    let mut bbox = BBox::empty();
    let mut layers: BTreeMap<String, u32> = BTreeMap::new();
    let mut geom_buf = Vec::new();
    let mut doc_buf = Vec::new();
    let mut tags = TagTable::default();
    let cc_upper = cc.to_ascii_uppercase();
    // Global admin index -> index in this partition's admin table.
    let mut remap: HashMap<u32, u32> = HashMap::new();
    let mut admins: Vec<AdminUnit> = Vec::new();

    // Decode on all cores in chunks, write sequentially in order.
    let mut id = 0usize;
    reader.for_each_chunk(&spill, |decoded| {
        for mut place in decoded {
            let place = &mut place;
            place.parents.retain(|&u| (u as usize) < input.admins.len());
            for u in &mut place.parents {
                *u = *remap.entry(*u).or_insert_with(|| {
                    admins.push(input.admins[*u as usize].clone());
                    (admins.len() - 1) as u32
                });
            }
            doc_buf.clear();
            doc::encode_stored(place, &mut tags, &cc_upper, &mut doc_buf)
                .map_err(|e| IndexError::Format(e.to_string()))?;
            docs.write_all(&doc_buf)?;

            geom_buf.clear();
            let (geom_kind, geom_len) = if !place.lines.is_empty() {
                (
                    GeomKind::Lines,
                    storage::encode_lines(&place.lines, &mut geom_buf),
                )
            } else if !place.polygons.is_empty() {
                (
                    GeomKind::Polygons,
                    storage::encode_polygons(&place.polygons, &mut geom_buf),
                )
            } else {
                (GeomKind::None, 0)
            };
            geom.write_all(&geom_buf)?;

            let mut rec = PlaceRecord {
                lon_e7: 0,
                lat_e7: 0,
                doc_offset,
                doc_len: doc_buf.len() as u32,
                geom_offset,
                geom_len,
                importance: place.importance,
                layer: place.layer,
                geom_kind,
            };
            rec.set_center(place.center);
            places_out.write_all(&rec.encode())?;
            importance.write_all(&rec.importance.to_le_bytes())?;
            doc_offset += doc_buf.len() as u64;
            geom_offset += geom_buf.len() as u64;

            let b = place.bbox();
            for v in [
                f32_down(b.min_lon),
                f32_down(b.min_lat),
                f32_up(b.max_lon),
                f32_up(b.max_lat),
            ] {
                boxes.write_all(&v.to_le_bytes())?;
            }
            bbox.union(&b);
            *layers.entry(place.layer.as_str().to_string()).or_default() += 1;

            // Context: names of parents in the default language plus English.
            let context: Vec<&str> = place
                .parents
                .iter()
                .filter_map(|&i| admins.get(i as usize))
                .flat_map(|a| {
                    std::iter::once(a.name.as_str()).chain(a.names.get("en").map(String::as_str))
                })
                .collect();
            text.add(id as u32, place, &context)?;
            id += 1;
        }
        Ok(())
    })?;
    drop(reader);
    geors_core::mem::mark("writer: places written");
    docs.flush()?;
    places_out.flush()?;
    importance.flush()?;
    geom.flush()?;
    boxes.flush()?;
    drop(boxes);
    text.finish()?;
    geors_core::mem::mark("writer: text index merged");

    // Streamed: leaves sorted on disk, only inner nodes in RAM.
    crate::rtree_writer::write(
        &boxes_path,
        count as usize,
        &tmp_dir.join(storage::SPATIAL_FILE),
        &tmp_dir,
    )?;
    fs::remove_file(&boxes_path)?;
    geors_core::mem::mark("writer: R-tree built");

    fs::write(
        tmp_dir.join(storage::TAGS_FILE),
        serde_json::to_vec(&tags.pairs)?,
    )?;
    fs::write(
        tmp_dir.join(storage::ADMIN_FILE),
        serde_json::to_vec(&admins)?,
    )?;
    let meta = PartitionMeta {
        format_version: FORMAT_VERSION,
        country_code: cc.clone(),
        country_name: input.country_name.clone(),
        num_places: count as u32,
        bbox,
        layers,
        source: input.source.clone(),
        created_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    fs::write(
        tmp_dir.join(storage::META_FILE),
        serde_json::to_vec_pretty(&meta)?,
    )?;

    if final_dir.exists() {
        warn!(partition = %cc, "replacing existing partition {}", final_dir.display());
        let old = data_dir.join(format!(".{cc}.old-{}", std::process::id()));
        fs::rename(&final_dir, &old)?;
        fs::rename(&tmp_dir, &final_dir)?;
        fs::remove_dir_all(&old)?;
    } else {
        fs::rename(&tmp_dir, &final_dir)?;
    }
    info!(
        partition = %cc,
        places = count,
        admins = admins.len(),
        elapsed = ?t.elapsed(),
        "wrote partition {}",
        final_dir.display()
    );
    Ok(final_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geors_core::{Layer, LonLat, OsmType};

    fn place(id: i64) -> Place {
        let lon = (id * 37 % 360) as f64 - 180.0;
        let lat = (id * 11 % 180) as f64 - 90.0;
        Place {
            osm_type: OsmType::Node,
            osm_id: id,
            osm_key: "place".into(),
            osm_value: "village".into(),
            layer: Layer::City,
            // Varying record sizes.
            name: Some("x".repeat(id as usize % 50)),
            names: Default::default(),
            alt_names: vec![],
            housenumber: None,
            street: None,
            postcode: None,
            city: None,
            parents: vec![],
            country_code: Some("LI".into()),
            center: LonLat::new(lon, lat),
            extent: None,
            importance: 0.5,
            lines: Vec::new(),
            polygons: Vec::new(),
            merged_ids: vec![],
            nominatim_id: None,
        }
    }

    fn read_all(o: &mut Ordered, spill: &SpillReader) -> Vec<Place> {
        let mut out = Vec::new();
        o.for_each_chunk(spill, |c| {
            out.extend(c);
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn bucketed_reads_match_direct_reads() {
        let dir = WorkDir::create(&std::env::temp_dir(), "geors-writer-test").unwrap();
        let mut w = SpillWriter::create(&dir.path.join("p.spill")).unwrap();
        // Reverse id order, so file order differs from sort order.
        for id in (0..5000).rev() {
            w.push(&place(id)).unwrap();
        }
        let spill = SpillReader::open(&w.finish().unwrap()).unwrap();
        let direct = read_all(
            &mut Ordered::open(&spill, true, 0, &dir.path).unwrap(),
            &spill,
        );
        assert_eq!(direct.len(), 5000);
        // Spatial order, ties by OSM id.
        let sort_key = |p: &Place| (morton(p.center.lon, p.center.lat), p.osm_id);
        assert!(
            direct
                .windows(2)
                .all(|w| sort_key(&w[0]) <= sort_key(&w[1]))
        );
        // Tiny buckets: many of them, and records larger than the share of
        // buffer per bucket.
        let mut bucketed = Ordered::open(&spill, false, 4096, &dir.path).unwrap();
        let Ordered::Buckets { buckets, .. } = &bucketed else {
            unreachable!()
        };
        assert!(buckets.len() > 20);
        assert_eq!(read_all(&mut bucketed, &spill), direct);
        drop(bucketed);
        let left: Vec<_> = std::fs::read_dir(&dir.path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, vec!["p.spill"], "scratch files removed");
    }
}
