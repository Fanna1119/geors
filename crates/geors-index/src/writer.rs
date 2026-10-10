//! Writes a country partition to disk (see `geors_core::storage` for the layout).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;

use geo_index::rtree::sort::HilbertSort;
use geo_index::rtree::{RTreeBuilder, RTreeIndex};
use geors_core::doc::{self, TagTable};
use geors_core::geom::{f32_down, f32_up};
use geors_core::spill::{Spill, SpillReader, SpillWriter, WorkDir};
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
/// Memory use is independent of the partition size apart from a 12 byte
/// sort key per place: places are streamed from the spill file in spatial
/// (Z-order) order.
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

    let spill = SpillReader::open(&input.places)?;
    // Spatial order; ties broken by OSM id so the output is deterministic
    // whatever order the import produced places in.
    let mut order: Vec<(u32, u8, i64, u64)> = Vec::with_capacity(count as usize);
    spill.for_each_entry(|e| {
        order.push((
            morton(e.center.lon, e.center.lat),
            e.osm_type,
            e.osm_id,
            e.offset,
        ))
    })?;
    order.sort_unstable();

    let mut docs =
        BufWriter::with_capacity(1 << 20, fs::File::create(tmp_dir.join(storage::DOCS_FILE))?);
    let mut places_out = BufWriter::with_capacity(
        1 << 20,
        fs::File::create(tmp_dir.join(storage::PLACES_FILE))?,
    );
    let mut geom =
        BufWriter::with_capacity(1 << 20, fs::File::create(tmp_dir.join(storage::GEOM_FILE))?);
    let mut text = TextIndexWriter::create(&tmp_dir.join(storage::TEXT_DIR))?;
    // f32 boxes, rounded outward: half the memory and disk of f64, and a
    // query can only get extra candidates, never miss one.
    let mut rtree = RTreeBuilder::<f32>::new(count as u32);

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
    for chunk in order.chunks(8192) {
        let decoded: Vec<Place> = chunk
            .par_iter()
            .map(|&(_, _, _, offset)| spill.get(offset))
            .collect::<std::io::Result<_>>()?;
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
            doc_offset += doc_buf.len() as u64;
            geom_offset += geom_buf.len() as u64;

            let b = place.bbox();
            rtree.add(
                f32_down(b.min_lon),
                f32_down(b.min_lat),
                f32_up(b.max_lon),
                f32_up(b.max_lat),
            );
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
    }
    docs.flush()?;
    places_out.flush()?;
    geom.flush()?;

    let tree = rtree.finish::<HilbertSort>();
    debug_assert_eq!(tree.num_items() as u64, count);
    fs::write(tmp_dir.join(storage::SPATIAL_FILE), tree.into_inner())?;
    text.finish()?;

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
