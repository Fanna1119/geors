//! Registry of imported sources (`<data>/sources.json`): where each extract
//! came from, how it was imported, and which version is currently loaded.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const SOURCES_FILE: &str = "sources.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// OpenStreetMap `.osm.pbf`
    Pbf,
    /// Nominatim / Photon JSON dump (`.json`, `.jsonl`, optionally `.gz`)
    Dump,
}

impl Format {
    pub fn detect(location: &str) -> Result<Self> {
        let l = location.to_ascii_lowercase();
        let l = l.split(['?', '#']).next().unwrap_or_default();
        if l.ends_with(".pbf") {
            Ok(Format::Pbf)
        } else if [".json", ".jsonl", ".json.gz", ".jsonl.gz"]
            .iter()
            .any(|e| l.ends_with(e))
        {
            Ok(Format::Dump)
        } else {
            bail!(
                "cannot tell the format of '{location}': expected .osm.pbf, .json, .jsonl or .jsonl.gz"
            )
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Pbf => "osm.pbf",
            Format::Dump => "jsonl.gz",
        }
    }
}

/// Identifies one published version of a source. Fields are filled as far
/// as the source can tell; two versions differ if any shared field differs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// Replication sequence number (Geofabrik `state.txt`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    /// Data timestamp, as published by the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Local files: modification time (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<u64>,
}

impl Version {
    /// Whether `other` is a different version than `self`. Uses the most
    /// reliable identifier both sides have; unknown means "changed".
    pub fn differs_from(&self, other: &Version) -> bool {
        fn cmp<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> Option<bool> {
            Some(a.as_ref()? != b.as_ref()?)
        }
        cmp(&self.sequence, &other.sequence)
            .or_else(|| cmp(&self.md5, &other.md5))
            .or_else(|| cmp(&self.etag, &other.etag))
            .or_else(|| {
                let lm = cmp(&self.last_modified, &other.last_modified)?;
                Some(lm || cmp(&self.size, &other.size).unwrap_or(false))
            })
            .or_else(|| {
                let mt = cmp(&self.mtime, &other.mtime)?;
                Some(mt || cmp(&self.size, &other.size).unwrap_or(false))
            })
            .unwrap_or(true)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceState {
    /// Version currently imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Version>,
    #[serde(default)]
    pub partitions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Local PBF that replication diffs are applied to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Replication state of `base`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replication: Option<ReplicationState>,
}

/// Where a base file's diffs come from and how far it is applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicationState {
    pub base_url: String,
    pub sequence: u64,
    /// Unix seconds of the data.
    pub timestamp: i64,
}

impl From<crate::pbf_write::Replication> for ReplicationState {
    fn from(r: crate::pbf_write::Replication) -> Self {
        Self {
            base_url: r.base_url,
            sequence: r.sequence,
            timestamp: r.timestamp,
        }
    }
}

impl From<&ReplicationState> for crate::pbf_write::Replication {
    fn from(r: &ReplicationState) -> Self {
        Self {
            base_url: r.base_url.clone(),
            sequence: r.sequence,
            timestamp: r.timestamp,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    /// URL (http/https) or absolute local path.
    pub location: String,
    pub format: Format,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub countries: Vec<String>,
    #[serde(default)]
    pub all_countries: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_country: Option<String>,
    /// Keep the PBF and apply replication diffs instead of downloading the
    /// whole extract again (when the extract publishes diffs).
    #[serde(default = "yes")]
    pub diffs: bool,
    #[serde(default)]
    pub state: SourceState,
}

fn yes() -> bool {
    true
}

impl Source {
    pub fn is_remote(&self) -> bool {
        is_url(&self.location)
    }

    pub fn import_options(&self) -> geors_ingest::ImportOptions {
        geors_ingest::ImportOptions {
            countries: self.countries.clone(),
            all_countries: self.all_countries,
            default_country: self.default_country.clone(),
            ..Default::default()
        }
    }
}

pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Expand shorthands: `geofabrik:europe/liechtenstein` ->
/// `https://download.geofabrik.de/europe/liechtenstein-latest.osm.pbf`.
pub fn expand_location(input: &str) -> String {
    match input.strip_prefix("geofabrik:") {
        Some(region) => format!(
            "https://download.geofabrik.de/{}-latest.osm.pbf",
            region.trim_matches('/')
        ),
        None => input.to_string(),
    }
}

/// A short, stable name: `liechtenstein-latest.osm.pbf` -> `liechtenstein`.
pub fn default_name(location: &str) -> String {
    let file = location
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("source");
    let mut name = file.to_ascii_lowercase();
    for suffix in [".gz", ".jsonl", ".json", ".pbf", ".osm", "-latest"] {
        if let Some(s) = name.strip_suffix(suffix) {
            name = s.to_string();
        }
    }
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() {
        "source".into()
    } else {
        name
    }
}

/// `sources.json`, loaded and saved as a whole.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub sources: Vec<Source>,
    #[serde(skip)]
    path: PathBuf,
}

impl Registry {
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(SOURCES_FILE);
        let mut reg: Registry = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::default(),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        reg.path = path;
        Ok(reg)
    }

    pub fn save(&self) -> Result<()> {
        let tmp = self
            .path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("cannot write {}", self.path.display()))?;
        Ok(())
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Source> {
        self.sources.iter_mut().find(|s| s.name == name)
    }

    /// Insert or replace (by name, or by identical location).
    pub fn upsert(&mut self, source: Source) {
        self.sources
            .retain(|s| s.name != source.name && s.location != source.location);
        self.sources.push(source);
        self.sources.sort_by(|a, b| a.name.cmp(&b.name));
    }

    pub fn remove(&mut self, name: &str) -> Option<Source> {
        let i = self.sources.iter().position(|s| s.name == name)?;
        Some(self.sources.remove(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_shorthands() {
        assert_eq!(
            expand_location("geofabrik:europe/liechtenstein"),
            "https://download.geofabrik.de/europe/liechtenstein-latest.osm.pbf"
        );
        assert_eq!(
            default_name("https://download.geofabrik.de/europe/liechtenstein-latest.osm.pbf"),
            "liechtenstein"
        );
        assert_eq!(default_name("/tmp/photon-dump.jsonl.gz"), "photon-dump");
        assert_eq!(Format::detect("x.osm.pbf").unwrap(), Format::Pbf);
        assert_eq!(Format::detect("x.jsonl.gz").unwrap(), Format::Dump);
        assert!(Format::detect("x.csv").is_err());
    }

    #[test]
    fn version_comparison() {
        let a = Version {
            sequence: Some(1),
            md5: Some("x".into()),
            ..Default::default()
        };
        let b = Version {
            sequence: Some(1),
            md5: Some("y".into()),
            ..Default::default()
        };
        // Sequence wins when both have it.
        assert!(!a.differs_from(&b));
        let c = Version {
            sequence: Some(2),
            ..Default::default()
        };
        assert!(a.differs_from(&c));
        let e1 = Version {
            etag: Some("e".into()),
            ..Default::default()
        };
        assert!(!e1.differs_from(&e1.clone()));
        // Nothing in common: assume changed.
        assert!(e1.differs_from(&Version::default()));
        let f1 = Version {
            mtime: Some(5),
            size: Some(10),
            ..Default::default()
        };
        let f2 = Version {
            mtime: Some(5),
            size: Some(11),
            ..Default::default()
        };
        assert!(f1.differs_from(&f2));
    }
}
