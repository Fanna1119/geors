//! Query execution across loaded country partitions.
//!
//! Search pipeline:
//! 1. **Partition pruning**: `country=` selects partitions; partitions whose
//!    extent misses the spatial filter are skipped.
//! 2. **Hard spatial filter**: radius / bbox candidates come from the packed
//!    R-tree and are checked exactly (geometry-aware distance).
//! 3. **Text search** runs only over that candidate set (a bitset handed to
//!    tantivy), scored by text x importance x proximity.
//! 4. **Re-ranking** of the merged top hits with name-match boosts, dedup.

use std::cmp::Ordering;
use std::path::Path;
use std::sync::Arc;

use geors_core::geom::{BBox, LonLat};
use geors_core::{AdminUnit, Layer, Place};
use geors_rank::RankingConfig;
use tantivy::tokenizer::TextAnalyzer;
use tracing::{debug, info};

use crate::bitset::BitSet;
use crate::partition::Partition;
use crate::text::{self, ParsedQuery};
use crate::{EngineError, IndexError};

/// Free-text and/or spatial search.
#[derive(Debug, Clone, Default)]
pub struct SearchRequest {
    pub query: Option<String>,
    /// Focus point: biases ranking, and is the centre for `radius_m`.
    pub focus: Option<LonLat>,
    /// Hard filter: only places within this distance of `focus`.
    pub radius_m: Option<f64>,
    /// Hard filter: only places intersecting this box.
    pub bbox: Option<BBox>,
    /// Lowercase ISO codes; empty means all loaded partitions.
    pub countries: Vec<String>,
    /// Empty means all layers.
    pub layers: Vec<Layer>,
    pub limit: usize,
    /// Treat the last query word as a prefix (search-as-you-type).
    pub autocomplete: bool,
}

/// Nearest places to a point (used by `/nearest` and `/reverse`).
#[derive(Debug, Clone)]
pub struct NearestRequest {
    pub point: LonLat,
    pub max_distance_m: Option<f64>,
    pub countries: Vec<String>,
    pub layers: Vec<Layer>,
    pub limit: usize,
}

/// A ranked result with its document loaded.
pub struct Hit {
    pub partition: Arc<Partition>,
    pub place_id: u32,
    pub place: Place,
    pub score: f32,
    pub distance_m: Option<f64>,
}

/// Address hierarchy resolved for one language.
#[derive(Debug, Default, Clone)]
pub struct Address {
    pub district: Option<String>,
    pub city: Option<String>,
    pub county: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
    pub country_code: String,
}

impl Hit {
    pub fn address(&self, lang: Option<&str>) -> Address {
        let mut a = Address {
            country_code: self
                .place
                .country_code
                .clone()
                .unwrap_or_else(|| self.partition.code().to_ascii_uppercase()),
            ..Default::default()
        };
        for unit in self
            .place
            .parents
            .iter()
            .filter_map(|&i| self.partition.admins.get(i as usize))
        {
            let slot = match unit.layer {
                Layer::District => &mut a.district,
                Layer::City => &mut a.city,
                Layer::County => &mut a.county,
                Layer::State => &mut a.state,
                Layer::Country => &mut a.country,
                _ => continue,
            };
            *slot = Some(AdminUnit::localized(unit, lang).to_string());
        }
        if a.city.is_none() {
            a.city = self.place.city.clone();
        }
        if a.country.is_none() && self.place.layer != Layer::Country {
            a.country = self.partition.meta.country_name.clone();
        }
        a
    }
}

/// Internal candidate before documents are loaded.
#[derive(Clone)]
struct Candidate {
    part: usize,
    id: u32,
    score: f32,
    distance_m: Option<f64>,
}

pub struct Engine {
    partitions: Vec<Arc<Partition>>,
    ranking: RankingConfig,
    analyzer: TextAnalyzer,
}

impl Engine {
    /// Open the partitions in `data_dir`. If `countries` is non-empty only
    /// those are loaded (and each must exist).
    pub fn open(
        data_dir: &Path,
        countries: &[String],
        ranking: RankingConfig,
    ) -> Result<Self, IndexError> {
        let available = list_partitions(data_dir)?;
        if available.is_empty() {
            return Err(IndexError::Format(format!(
                "no partitions found in '{}'; run `geors import <file.osm.pbf> --data {}` first",
                data_dir.display(),
                data_dir.display()
            )));
        }
        let selected: Vec<String> = if countries.is_empty() {
            available.clone()
        } else {
            for cc in countries {
                if !available.contains(cc) {
                    return Err(IndexError::Format(format!(
                        "country '{cc}' not found in '{}' (available: {})",
                        data_dir.display(),
                        available.join(", ")
                    )));
                }
            }
            countries.to_vec()
        };
        let mut partitions = Vec::new();
        for cc in selected {
            let p = Partition::open(&data_dir.join(&cc))?;
            info!(partition = %cc, places = p.len(), source = %p.meta.source, "loaded partition");
            partitions.push(Arc::new(p));
        }
        Ok(Self::from_partitions(partitions, ranking))
    }

    pub fn from_partitions(partitions: Vec<Arc<Partition>>, ranking: RankingConfig) -> Self {
        Self {
            partitions,
            ranking,
            analyzer: text::analyzer(),
        }
    }

    pub fn partitions(&self) -> &[Arc<Partition>] {
        &self.partitions
    }

    fn select(&self, countries: &[String]) -> Result<Vec<usize>, EngineError> {
        if countries.is_empty() {
            return Ok((0..self.partitions.len()).collect());
        }
        countries
            .iter()
            .map(|cc| {
                self.partitions
                    .iter()
                    .position(|p| p.code() == cc)
                    .ok_or_else(|| EngineError::CountryNotLoaded {
                        requested: cc.clone(),
                        loaded: self
                            .partitions
                            .iter()
                            .map(|p| p.code().to_string())
                            .collect(),
                    })
            })
            .collect()
    }

    pub fn search(&self, req: &SearchRequest) -> Result<Vec<Hit>, EngineError> {
        let limit = req.limit.max(1);
        if req.radius_m.is_some() && req.focus.is_none() {
            return Err(EngineError::BadRequest(
                "'radius' requires 'lat' and 'lon'".into(),
            ));
        }
        if let Some(r) = req.radius_m
            && !(r > 0.0 && r.is_finite())
        {
            return Err(EngineError::BadRequest(
                "'radius' must be a positive number of metres".into(),
            ));
        }
        let parsed = req
            .query
            .as_deref()
            .map(|q| ParsedQuery::parse(&mut self.analyzer.clone(), q, req.autocomplete))
            .filter(|p| !p.tokens.is_empty());

        // Hard spatial filter region (intersection of bbox and radius box).
        let filter_box = match (req.bbox, req.focus.zip(req.radius_m)) {
            (Some(b), Some((c, r))) => Some(intersect(&b, &BBox::around(c, r))),
            (Some(b), None) => Some(b),
            (None, Some((c, r))) => Some(BBox::around(c, r)),
            (None, None) => None,
        };

        let Some(parsed) = parsed else {
            return match (filter_box, req.focus) {
                (Some(fb), _) => self.spatial_only(req, &fb, limit),
                (None, Some(point)) => self.nearest(&NearestRequest {
                    point,
                    max_distance_m: None,
                    countries: req.countries.clone(),
                    layers: req.layers.clone(),
                    limit,
                }),
                (None, None) => Err(EngineError::BadRequest(
                    "provide a query 'q', a location ('lat'/'lon'), or a 'bbox'".into(),
                )),
            };
        };

        let parts = self.select(&req.countries)?;
        let fetch = (limit * 5).clamp(20, 200);
        let mut cands = Vec::new();
        for slack in [0usize, 1] {
            if slack > 0 && (parsed.tokens.len() < 2 || cands.len() >= limit) {
                break;
            }
            // Relaxed matches (one word missing) are penalised.
            let penalty = if slack == 0 { 1.0 } else { 0.5 };
            for &pi in &parts {
                let part = &self.partitions[pi];
                let filter = match &filter_box {
                    Some(fb) => {
                        let set = self.spatial_candidates(part, fb, req)?;
                        if set.is_empty() {
                            continue;
                        }
                        Some(Arc::new(set))
                    }
                    None => None,
                };
                let query = part.text.build_query(&parsed, &req.layers, slack);
                let scorer_part = part.clone();
                let ranking = self.ranking.clone();
                let focus = req.focus;
                let scorer = Arc::new(move |id: u32, s: f32| {
                    let Some(rec) = scorer_part.record(id) else {
                        return 0.0;
                    };
                    let d = focus.map(|f| geors_core::geom::haversine(f, rec.center()));
                    ranking.combined(s, rec.importance, d) * penalty
                });
                let hits = part
                    .text
                    .search(query.as_ref(), filter, scorer, fetch)
                    .map_err(IndexError::from)?;
                for (score, id) in hits {
                    if !cands.iter().any(|c: &Candidate| c.part == pi && c.id == id) {
                        cands.push(Candidate {
                            part: pi,
                            id,
                            score,
                            distance_m: None,
                        });
                    }
                }
            }
        }
        debug!(candidates = cands.len(), tokens = ?parsed.tokens, "text search");
        sort_by_score(&mut cands);
        cands.truncate(fetch);

        let mut hits = self.load(cands, req.focus)?;
        let mut analyzer = self.analyzer.clone();
        for hit in &mut hits {
            let boost = hit
                .place
                .all_names()
                .map(|n| {
                    let name_tokens = text::tokenize(&mut analyzer, n);
                    self.ranking
                        .name_match(&parsed.tokens, &name_tokens, parsed.last_is_prefix)
                })
                .fold(1.0f32, f32::max);
            hit.score *= boost;
        }
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        Ok(dedup(hits, limit))
    }

    /// Place ids in `part` passing the hard spatial filter and layer filter.
    fn spatial_candidates(
        &self,
        part: &Partition,
        fb: &BBox,
        req: &SearchRequest,
    ) -> Result<BitSet, EngineError> {
        let mut set = BitSet::new(part.len() as usize);
        for id in part.search_bbox(fb)? {
            let Some(rec) = part.record(id) else { continue };
            if !req.layers.is_empty() && !req.layers.contains(&rec.layer) {
                continue;
            }
            if let (Some(c), Some(r)) = (req.focus, req.radius_m)
                && part.distance(&rec, c) > r
            {
                continue;
            }
            set.insert(id);
        }
        Ok(set)
    }

    /// No query text: everything inside the filter, nearest first when a
    /// focus point is given, otherwise most important first.
    fn spatial_only(
        &self,
        req: &SearchRequest,
        fb: &BBox,
        limit: usize,
    ) -> Result<Vec<Hit>, EngineError> {
        let mut cands = Vec::new();
        for pi in self.select(&req.countries)? {
            let part = &self.partitions[pi];
            for id in part.search_bbox(fb)? {
                let Some(rec) = part.record(id) else { continue };
                if !req.layers.is_empty() && !req.layers.contains(&rec.layer) {
                    continue;
                }
                let distance_m = req.focus.map(|c| part.distance(&rec, c));
                if let (Some(d), Some(r)) = (distance_m, req.radius_m)
                    && d > r
                {
                    continue;
                }
                let score = match distance_m {
                    Some(d) => self.ranking.proximity(d) * (1.0 + rec.importance * 1e-3),
                    None => rec.importance,
                };
                cands.push(Candidate {
                    part: pi,
                    id,
                    score,
                    distance_m,
                });
            }
        }
        sort_by_score(&mut cands);
        cands.truncate(limit);
        self.load(cands, req.focus)
    }

    /// Exact k-nearest neighbours by true (geometry-aware) distance.
    ///
    /// The R-tree yields approximate neighbours; the k-th best exact distance
    /// among them bounds the answer, so a second, exact pass over that radius
    /// gives the correct result.
    pub fn nearest(&self, req: &NearestRequest) -> Result<Vec<Hit>, EngineError> {
        let limit = req.limit.max(1);
        let p = req.point;
        let passes = |layer: Layer| req.layers.is_empty() || req.layers.contains(&layer);
        let mut cands: Vec<Candidate> = Vec::new();
        for pi in self.select(&req.countries)? {
            let part = &self.partitions[pi];
            let radius = match req.max_distance_m {
                Some(r) => Some(r),
                None => {
                    // Grow the approximate search until it yields `limit` matches.
                    let mut want = (limit * 4).max(16);
                    loop {
                        let ids = part.approx_neighbors(p, want)?;
                        let mut ds: Vec<f64> = ids
                            .iter()
                            .filter_map(|&id| part.record(id))
                            .filter(|r| passes(r.layer))
                            .map(|r| part.distance(&r, p))
                            .collect();
                        let exhausted = ids.len() < want;
                        if ds.len() >= limit {
                            ds.sort_by(f64::total_cmp);
                            break Some(ds[limit - 1]);
                        }
                        if exhausted {
                            // Every item was examined: no radius bound needed.
                            break ds.into_iter().max_by(f64::total_cmp);
                        }
                        want *= 4;
                    }
                }
            };
            let Some(radius) = radius else { continue };
            for id in part.search_bbox(&BBox::around(p, radius))? {
                let Some(rec) = part.record(id) else { continue };
                if !passes(rec.layer) {
                    continue;
                }
                let d = part.distance(&rec, p);
                if d <= radius {
                    cands.push(Candidate {
                        part: pi,
                        id,
                        score: 0.0,
                        distance_m: Some(d),
                    });
                }
            }
        }
        cands.sort_by(|a, b| {
            a.distance_m
                .partial_cmp(&b.distance_m)
                .unwrap_or(Ordering::Equal)
        });
        cands.truncate(limit);
        for c in &mut cands {
            c.score = self.ranking.proximity(c.distance_m.unwrap_or(0.0));
        }
        self.load(cands, Some(p))
    }

    fn load(&self, cands: Vec<Candidate>, focus: Option<LonLat>) -> Result<Vec<Hit>, EngineError> {
        cands
            .into_iter()
            .filter_map(|c| {
                let part = self.partitions[c.part].clone();
                let rec = part.record(c.id)?;
                let distance_m = c
                    .distance_m
                    .or_else(|| focus.map(|f| part.distance(&rec, f)));
                Some(part.doc(&rec).map(|place| Hit {
                    place,
                    place_id: c.id,
                    score: c.score,
                    distance_m,
                    partition: part.clone(),
                }))
            })
            .collect::<Result<_, _>>()
            .map_err(EngineError::from)
    }
}

fn sort_by_score(cands: &mut [Candidate]) {
    cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
}

fn intersect(a: &BBox, b: &BBox) -> BBox {
    BBox::new(
        a.min_lon.max(b.min_lon),
        a.min_lat.max(b.min_lat),
        a.max_lon.min(b.max_lon),
        a.max_lat.min(b.max_lat),
    )
}

/// Drop near-duplicates: same layer, name and house number within 1 km
/// (e.g. a town's place node and its boundary, or unmerged street pieces).
fn dedup(hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    let mut kept: Vec<Hit> = Vec::with_capacity(limit);
    for hit in hits {
        let dup = kept.iter().any(|k| {
            k.place.layer == hit.place.layer
                && k.place.name.is_some()
                && k.place.name == hit.place.name
                && k.place.housenumber == hit.place.housenumber
                && geors_core::geom::haversine(k.place.center, hit.place.center) < 1_000.0
        });
        if !dup {
            kept.push(hit);
            if kept.len() == limit {
                break;
            }
        }
    }
    kept
}

/// Partition directory names (lowercase country codes) in `data_dir`.
pub fn list_partitions(data_dir: &Path) -> Result<Vec<String>, IndexError> {
    let entries = std::fs::read_dir(data_dir).map_err(|e| {
        IndexError::Format(format!(
            "cannot read data directory '{}': {e}",
            data_dir.display()
        ))
    })?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with('.') && entry.path().join(geors_core::storage::META_FILE).is_file() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}
