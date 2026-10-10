//! Routes finished places into one spill file per country and decides,
//! at the end, which countries become partitions.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use geors_core::spill::SpillWriter;
use geors_core::{AdminUnit, Place};
use tracing::{info, warn};

use crate::{CountryData, ImportOptions};

/// How completely an extract covers a country (see [`CountrySink::finish`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Coverage {
    /// Complete boundaries of regions (`ISO3166-2`) inside the extract.
    Regions,
    /// The complete country boundary lies inside the extract.
    Boundary,
}

pub struct CountrySink {
    dir: PathBuf,
    writers: BTreeMap<String, SpillWriter>,
    default_country: Option<String>,
    unknown: u64,
}

impl CountrySink {
    pub fn new(dir: PathBuf, default_country: Option<String>) -> Self {
        Self {
            dir,
            writers: BTreeMap::new(),
            default_country,
            unknown: 0,
        }
    }

    /// Places without a country (and no default) are dropped and counted.
    pub fn push(&mut self, mut place: Place) -> io::Result<()> {
        let Some(cc) = place
            .country_code
            .as_deref()
            .and_then(geors_core::normalize_country_code)
            .or_else(|| self.default_country.clone())
        else {
            self.unknown += 1;
            return Ok(());
        };
        place.country_code = Some(cc.to_ascii_uppercase());
        let w = match self.writers.get_mut(&cc) {
            Some(w) => w,
            None => {
                let w = SpillWriter::create(&self.dir.join(format!("{cc}.spill")))?;
                self.writers.entry(cc).or_insert(w)
            }
        };
        w.push(&place)?;
        Ok(())
    }

    pub fn total(&self) -> u64 {
        self.writers.values().map(SpillWriter::count).sum()
    }

    /// Apply the country selection rules and return the partitions to write.
    ///
    /// Unless countries are chosen explicitly, a country is kept when
    /// - its complete boundary lies inside the extract, or
    /// - complete regions of it do, and it has at least a tenth of
    ///   `min_share` of the places (a region can be a single municipality
    ///   of a neighbour), or
    /// - it has at least `min_share` of the places.
    ///
    /// The rest are border slivers: the few neighbouring places that an
    /// extract's buffer pulls in. Writing them would replace a real import
    /// of that country with a fragment.
    pub fn finish(
        self,
        opts: &ImportOptions,
        admins: Vec<AdminUnit>,
        country_names: &HashMap<String, String>,
        covered: &HashMap<String, Coverage>,
    ) -> io::Result<Vec<CountryData>> {
        if self.unknown > 0 {
            warn!(
                unknown = self.unknown,
                "places outside any country boundary were skipped (use --default-country to keep them)"
            );
        }
        let total = self.total();
        let found: Vec<String> = self
            .writers
            .iter()
            .map(|(cc, w)| format!("{cc}={}", w.count()))
            .collect();
        info!(countries = %found.join(" "), "places per country");
        for cc in &opts.countries {
            if !self.writers.contains_key(cc) {
                warn!(country = %cc, "requested country has no places in this extract");
            }
        }
        let admins = Arc::new(admins);
        let mut out = Vec::new();
        for (cc, w) in self.writers {
            let keep = if !opts.countries.is_empty() {
                opts.countries.contains(&cc)
            } else if opts.all_countries {
                true
            } else {
                let share = w.count() as f64 / total.max(1) as f64;
                let keep = match covered.get(&cc) {
                    Some(Coverage::Boundary) => true,
                    Some(Coverage::Regions) => share >= opts.min_share / 10.0,
                    None => share >= opts.min_share,
                };
                if !keep {
                    info!(country = %cc, places = w.count(), "skipping border sliver (use --all-countries or --countries to keep it)");
                }
                keep
            };
            let spill = w.finish()?;
            if !keep {
                let _ = std::fs::remove_file(&spill.path);
                continue;
            }
            out.push(CountryData {
                country_name: country_names.get(&cc).cloned(),
                country_code: cc,
                places: spill,
                admins: admins.clone(),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geors_core::{Layer, LonLat, OsmType};

    fn place(cc: &str, id: i64) -> Place {
        Place {
            osm_type: OsmType::Node,
            osm_id: id,
            osm_key: "amenity".into(),
            osm_value: "cafe".into(),
            layer: Layer::Poi,
            name: Some("x".into()),
            names: Default::default(),
            alt_names: vec![],
            housenumber: None,
            street: None,
            postcode: None,
            city: None,
            parents: vec![],
            country_code: Some(cc.into()),
            center: LonLat::new(0.0, 0.0),
            extent: None,
            importance: 0.1,
            lines: Vec::new(),
            polygons: Vec::new(),
            merged_ids: vec![],
        }
    }

    #[test]
    fn keeps_covered_countries_and_drops_slivers() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = CountrySink::new(dir.path().to_path_buf(), None);
        // 100,000 places: 1 % = 1,000, a tenth of that = 100.
        let counts = [
            ("AT", 96_000), // main country
            ("DE", 1_500),  // >= 1 %: kept by share
            ("VA", 50),     // complete boundary inside the extract
            ("RU", 2_000),  // regions only, >= 0.1 %
            ("LI", 50),     // regions only, < 0.1 %: sliver
            ("CH", 400),    // nothing, < 1 %: sliver
        ];
        let mut id = 0;
        for (cc, n) in counts {
            for _ in 0..n {
                id += 1;
                sink.push(place(cc, id)).unwrap();
            }
        }
        let covered = HashMap::from([
            ("va".to_string(), Coverage::Boundary),
            ("ru".to_string(), Coverage::Regions),
            ("li".to_string(), Coverage::Regions),
        ]);
        let kept: Vec<String> = sink
            .finish(
                &ImportOptions::default(),
                Vec::new(),
                &HashMap::new(),
                &covered,
            )
            .unwrap()
            .into_iter()
            .map(|c| c.country_code)
            .collect();
        assert_eq!(kept, ["at", "de", "ru", "va"]);
    }
}
