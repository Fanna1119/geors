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
    pub fn finish(
        self,
        opts: &ImportOptions,
        admins: Vec<AdminUnit>,
        country_names: &HashMap<String, String>,
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
                let keep = w.count() as f64 >= opts.min_share * total as f64;
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
