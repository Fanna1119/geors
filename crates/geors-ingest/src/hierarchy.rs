//! The address hierarchy: which admin units, country and postcode a place
//! belongs to. Built during import from boundary polygons and `place=*`
//! nodes; can be kept on disk ([`Hierarchy::save`]) so incremental updates
//! assign parents to changed places exactly as the import did.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use geors_core::{AdminUnit, Layer, Place};
use serde::{Deserialize, Serialize};

use crate::areas::{Area, AreaIndex, CellCache, LocalityIndex, LocalityPoint};
use crate::pip::PolygonStore;

/// Files of a kept hierarchy (in its state directory).
pub const ADMIN_POLYGONS: &str = "admin-polygons";
pub const POSTCODE_POLYGONS: &str = "postcode-polygons";
const META: &str = "hierarchy.bin";
/// Admin units, as JSON like a partition's `admin.json` (`AdminUnit` skips
/// empty fields, which a non-self-describing format cannot decode).
const UNITS: &str = "units.json";

/// Everything but the units and the polygon arrays (those are in the
/// polygon stores).
#[derive(Serialize, Deserialize)]
struct Meta {
    areas: Vec<Area>,
    postcodes: Vec<Area>,
    postcode_names: Vec<String>,
    localities: Vec<LocalityPoint>,
}

/// Assigns parents (admin units), country and postcode to places.
pub struct Hierarchy {
    pub units: Vec<AdminUnit>,
    pub areas: AreaIndex,
    pub postcodes: AreaIndex,
    pub postcode_names: Vec<String>,
    pub localities: LocalityIndex,
}

/// Per-worker point-in-polygon caches.
#[derive(Default)]
pub struct HierCache {
    areas: CellCache,
    postcodes: CellCache,
}

impl Hierarchy {
    /// Read-only apart from the caller's cache, so workers run it in parallel.
    pub fn assign(&self, cache: &mut HierCache, place: &mut Place, own: Option<u32>) {
        let mut best: BTreeMap<Layer, (u8, u32)> = BTreeMap::new();
        let mut country = None;
        let mut region_country = None;
        for ai in self.areas.containing(&mut cache.areas, place.center) {
            let a = &self.areas.areas[ai];
            if let Some(cc) = &a.country_code {
                let slot = if a.layer == Layer::Country {
                    &mut country
                } else {
                    &mut region_country
                };
                if slot.is_none() {
                    *slot = Some(cc.clone());
                }
            }
            if a.layer <= place.layer || Some(a.unit) == own {
                continue;
            }
            let e = best.entry(a.layer).or_insert((a.rank, a.unit));
            if a.rank > e.0 {
                *e = (a.rank, a.unit);
            }
        }
        for layer in [Layer::City, Layer::District] {
            if place.layer < layer
                && !best.contains_key(&layer)
                && let Some(u) = self
                    .localities
                    .best(place.center, layer)
                    .filter(|u| Some(*u) != own)
            {
                best.insert(layer, (0, u));
            }
        }
        place.parents = best.values().map(|(_, u)| *u).collect();
        // A country boundary cut by the extract's edge is unusable, but the
        // regions inside it (states, provinces) usually are complete and
        // name their country in `ISO3166-2`.
        place.country_code = country.or(region_country).or(place.country_code.take());
        if place.postcode.is_none() && place.layer <= Layer::Street {
            place.postcode = self
                .postcodes
                .containing(&mut cache.postcodes, place.center)
                .first()
                .map(|&i| self.postcode_names[self.postcodes.areas[i].unit as usize].clone());
        }
    }

    pub fn city_of(&self, place: &Place) -> Option<u32> {
        place
            .parents
            .iter()
            .copied()
            .find(|&u| self.units[u as usize].layer == Layer::City)
    }
}

impl Hierarchy {
    /// Save the metadata to `dir`. The polygon stores must have been created
    /// there with [`PolygonStore::create_kept`] ([`ADMIN_POLYGONS`],
    /// [`POSTCODE_POLYGONS`]).
    pub fn save(&self, dir: &Path) -> Result<()> {
        let units_tmp = dir.join(format!("{UNITS}.tmp"));
        std::fs::write(&units_tmp, serde_json::to_vec(&self.units)?)?;
        std::fs::rename(&units_tmp, dir.join(UNITS))?;
        let meta = Meta {
            areas: self.areas.areas.clone(),
            postcodes: self.postcodes.areas.clone(),
            postcode_names: self.postcode_names.clone(),
            localities: self.localities.points.clone(),
        };
        let tmp = dir.join(format!("{META}.tmp"));
        std::fs::write(&tmp, postcard::to_allocvec(&meta)?)?;
        std::fs::rename(&tmp, dir.join(META))?;
        Ok(())
    }

    /// Load a hierarchy saved with [`Hierarchy::save`].
    pub fn load(dir: &Path) -> Result<Self> {
        let bytes = std::fs::read(dir.join(META))
            .with_context(|| format!("no kept hierarchy in '{}'", dir.display()))?;
        let meta: Meta = postcard::from_bytes(&bytes).context("corrupt kept hierarchy")?;
        let units: Vec<AdminUnit> = serde_json::from_slice(&std::fs::read(dir.join(UNITS))?)
            .context("corrupt kept admin units")?;
        Ok(Self {
            units,
            areas: AreaIndex::new(meta.areas, PolygonStore::open(&dir.join(ADMIN_POLYGONS))?)?,
            postcodes: AreaIndex::new(
                meta.postcodes,
                PolygonStore::open(&dir.join(POSTCODE_POLYGONS))?,
            )?,
            postcode_names: meta.postcode_names,
            localities: LocalityIndex::new(meta.localities),
        })
    }
}
