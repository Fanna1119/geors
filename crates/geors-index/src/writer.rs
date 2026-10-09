//! Writes a country partition to disk (see `geors_core::storage` for the layout).

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use geo_index::rtree::sort::HilbertSort;
use geo_index::rtree::{RTreeBuilder, RTreeIndex};
use geors_core::storage::{self, FORMAT_VERSION, GeomKind, PartitionMeta, PlaceRecord};
use geors_core::{AdminUnit, BBox, Layer, Place};
use tracing::{info, warn};

use crate::IndexError;
use crate::text::TextIndexWriter;

pub struct PartitionInput {
    pub country_code: String,
    pub country_name: Option<String>,
    pub source: String,
    pub places: Vec<Place>,
    pub admins: Vec<AdminUnit>,
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

/// Build all partition files in a temporary directory, then atomically swap
/// it into `<data_dir>/<cc>`.
pub fn write_partition(data_dir: &Path, mut input: PartitionInput) -> Result<PathBuf, IndexError> {
    let cc = input.country_code.clone();
    if input.places.is_empty() {
        return Err(IndexError::Format(format!(
            "partition '{cc}' has no places"
        )));
    }
    if input.places.len() > u32::MAX as usize {
        return Err(IndexError::Format(format!(
            "partition '{cc}' has too many places"
        )));
    }
    fs::create_dir_all(data_dir)?;
    let final_dir = storage::partition_dir(data_dir, &cc);
    let tmp_dir = data_dir.join(format!(".{cc}.tmp-{}", std::process::id()));
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir)?;
    }
    fs::create_dir_all(&tmp_dir)?;

    input
        .places
        .sort_by_key(|p| morton(p.center.lon, p.center.lat));

    let mut docs = BufWriter::new(fs::File::create(tmp_dir.join(storage::DOCS_FILE))?);
    let mut places_out = BufWriter::new(fs::File::create(tmp_dir.join(storage::PLACES_FILE))?);
    let mut geom = BufWriter::new(fs::File::create(tmp_dir.join(storage::GEOM_FILE))?);
    let mut text = TextIndexWriter::create(&tmp_dir.join(storage::TEXT_DIR))?;
    let mut rtree = RTreeBuilder::<f64>::new(input.places.len() as u32);

    let mut doc_offset = 0u64;
    let mut geom_offset = 0u64;
    let mut bbox = BBox::empty();
    let mut layers: BTreeMap<String, u32> = BTreeMap::new();
    let mut geom_buf = Vec::new();

    for (id, place) in input.places.iter().enumerate() {
        let json = serde_json::to_vec(place)?;
        docs.write_all(&json)?;

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
            doc_len: json.len() as u32,
            geom_offset,
            geom_len,
            importance: place.importance,
            layer: place.layer,
            geom_kind,
        };
        rec.set_center(place.center);
        places_out.write_all(&rec.encode())?;
        doc_offset += json.len() as u64;
        geom_offset += geom_buf.len() as u64;

        let b = place.bbox();
        rtree.add(b.min_lon, b.min_lat, b.max_lon, b.max_lat);
        bbox.union(&b);
        *layers.entry(place.layer.as_str().to_string()).or_default() += 1;

        // Context: names of parents in the default language plus English.
        let context: Vec<&str> = place
            .parents
            .iter()
            .filter_map(|&i| input.admins.get(i as usize))
            .flat_map(|a| {
                std::iter::once(a.name.as_str()).chain(a.names.get("en").map(String::as_str))
            })
            .collect();
        text.add(id as u32, place, &context)?;
    }
    docs.flush()?;
    places_out.flush()?;
    geom.flush()?;

    let tree = rtree.finish::<HilbertSort>();
    debug_assert_eq!(tree.num_items() as usize, input.places.len());
    fs::write(tmp_dir.join(storage::SPATIAL_FILE), tree.into_inner())?;
    text.finish()?;

    fs::write(
        tmp_dir.join(storage::ADMIN_FILE),
        serde_json::to_vec(&input.admins)?,
    )?;
    let meta = PartitionMeta {
        format_version: FORMAT_VERSION,
        country_code: cc.clone(),
        country_name: input.country_name.clone(),
        num_places: input.places.len() as u32,
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
        places = input.places.len(),
        admins = input.admins.len(),
        "wrote partition {}",
        final_dir.display()
    );
    Ok(final_dir)
}

/// Count of places per layer, for logging.
pub fn layer_histogram(places: &[Place]) -> BTreeMap<Layer, usize> {
    let mut h = BTreeMap::new();
    for p in places {
        *h.entry(p.layer).or_default() += 1;
    }
    h
}
