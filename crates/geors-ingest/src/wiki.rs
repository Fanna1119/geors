//! Importance from Wikipedia: Nominatim's `wikimedia-importance.csv.gz`
//! (<https://nominatim.org/data/wikimedia-importance.csv.gz>), one row per
//! Wikipedia article with an importance in 0..1 computed from the links
//! pointing to it, and the article's Wikidata id.
//!
//! The file is ~320 MB of gzip-compressed TSV (19 M rows, 3.6 M distinct
//! Wikidata ids). Parsing it takes a while, so the highest importance per
//! id is cached as a small binary file (8 bytes per id) next to the data.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use flate2::read::MultiGzDecoder;
use tracing::info;

/// Highest article importance per Wikidata id, sorted by id.
pub struct WikiImportance {
    ids: Vec<u32>,
    values: Vec<f32>,
}

impl std::fmt::Debug for WikiImportance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WikiImportance({} ids)", self.ids.len())
    }
}

/// Cache format: magic, source file size and mtime, count, ids, values.
const MAGIC: &[u8; 8] = b"GEORSWI1";

impl WikiImportance {
    /// Load `path` (the `.csv.gz` file), from a cache in `cache_dir` when
    /// one exists for this exact file, else by parsing it and writing the
    /// cache.
    pub fn load(path: &Path, cache_dir: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path)
            .with_context(|| format!("cannot read importance file '{}'", path.display()))?;
        let stamp = (
            meta.len(),
            meta.modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        let cache = cache_dir.join("wikimedia-importance.bin");
        if let Some(t) = Self::read_cache(&cache, stamp) {
            info!(ids = t.ids.len(), cache = %cache.display(), "loaded Wikipedia importance");
            return Ok(t);
        }
        let t = std::time::Instant::now();
        let table = Self::parse(path)?;
        info!(ids = table.ids.len(), elapsed = ?t.elapsed(), "parsed Wikipedia importance");
        if let Err(e) = table.write_cache(&cache, stamp) {
            tracing::warn!(error = %e, "cannot cache Wikipedia importance; it will be parsed again next time");
        }
        Ok(table)
    }

    /// Importance for a `wikidata` tag value ("Q64"), if it has an article.
    pub fn get(&self, wikidata: &str) -> Option<f32> {
        let id = parse_qid(wikidata)?;
        self.ids.binary_search(&id).ok().map(|i| self.values[i])
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    fn parse(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let reader = BufReader::with_capacity(1 << 20, MultiGzDecoder::new(file));
        let mut lines = reader.lines();
        let header = lines.next().transpose()?.unwrap_or_default();
        let cols: Vec<&str> = header.split('\t').collect();
        let col = |name: &str| cols.iter().position(|c| *c == name);
        let (Some(ci), Some(cw)) = (col("importance"), col("wikidata_id")) else {
            bail!(
                "'{}' is not a Wikipedia importance file (expected columns importance and wikidata_id, got: {header})",
                path.display()
            );
        };
        let mut best: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
        for line in lines {
            let line = line?;
            let mut fields = line.split('\t');
            let (mut imp, mut qid) = (None, None);
            for (i, f) in fields.by_ref().enumerate() {
                if i == ci {
                    imp = f.parse::<f32>().ok();
                } else if i == cw {
                    qid = parse_qid(f);
                }
            }
            if let (Some(imp), Some(qid)) = (imp, qid) {
                let e = best.entry(qid).or_insert(imp);
                *e = e.max(imp);
            }
        }
        let mut pairs: Vec<(u32, f32)> = best.into_iter().collect();
        pairs.sort_unstable_by_key(|p| p.0);
        Ok(Self {
            ids: pairs.iter().map(|p| p.0).collect(),
            values: pairs.iter().map(|p| p.1.clamp(0.0, 1.0)).collect(),
        })
    }

    fn read_cache(path: &Path, stamp: (u64, u64)) -> Option<Self> {
        let mut bytes = Vec::new();
        File::open(path).ok()?.read_to_end(&mut bytes).ok()?;
        let u64_at = |o: usize| Some(u64::from_le_bytes(bytes.get(o..o + 8)?.try_into().ok()?));
        if bytes.get(..8)? != MAGIC || (u64_at(8)?, u64_at(16)?) != stamp {
            return None;
        }
        let n = u64_at(24)? as usize;
        let body = bytes.get(32..32 + n * 8)?;
        let (ids, values) = body.split_at(n * 4);
        Some(Self {
            ids: ids
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect(),
            values: values
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                .collect(),
        })
    }

    fn write_cache(&self, path: &Path, stamp: (u64, u64)) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
        let mut out = std::io::BufWriter::new(File::create(&tmp)?);
        out.write_all(MAGIC)?;
        for v in [stamp.0, stamp.1, self.ids.len() as u64] {
            out.write_all(&v.to_le_bytes())?;
        }
        for id in &self.ids {
            out.write_all(&id.to_le_bytes())?;
        }
        for v in &self.values {
            out.write_all(&v.to_le_bytes())?;
        }
        out.flush()?;
        drop(out);
        std::fs::rename(tmp, path)
    }

    #[cfg(test)]
    pub(crate) fn from_pairs(mut pairs: Vec<(u32, f32)>) -> Self {
        pairs.sort_unstable_by_key(|p| p.0);
        Self {
            ids: pairs.iter().map(|p| p.0).collect(),
            values: pairs.iter().map(|p| p.1).collect(),
        }
    }
}

/// "Q64" -> 64. OSM `wikidata` tags sometimes hold several ids
/// ("Q1;Q2"); the first one counts.
fn parse_qid(s: &str) -> Option<u32> {
    s.split(';').next()?.trim().strip_prefix('Q')?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keeps_highest_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wiki.csv.gz");
        let rows = "language\ttype\ttitle\timportance\twikidata_id\n\
            en\ta\tBerlin\t0.85\tQ64\n\
            de\ta\tBerlin\t0.80\tQ64\n\
            en\ta\tSpringfield, Illinois\t0.65\tQ28848\n\
            en\ta\tNo id\t0.5\t\n";
        let mut gz =
            flate2::write::GzEncoder::new(File::create(&path).unwrap(), Default::default());
        gz.write_all(rows.as_bytes()).unwrap();
        gz.finish().unwrap();
        let cache = dir.path().join("cache");
        for _ in 0..2 {
            // First parsed, then from the cache.
            let t = WikiImportance::load(&path, &cache).unwrap();
            assert_eq!(t.len(), 2);
            assert_eq!(t.get("Q64"), Some(0.85));
            assert_eq!(t.get("Q28848;Q1"), Some(0.65));
            assert_eq!(t.get("Q1"), None);
            assert_eq!(t.get("64"), None);
        }
        assert!(cache.join("wikimedia-importance.bin").exists());
    }
}
