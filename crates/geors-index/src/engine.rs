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
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use geors_core::geom::{BBox, LonLat};
use geors_core::{AdminUnit, Layer, OsmType, Place};
use geors_rank::RankingConfig;
use rayon::prelude::*;
use tantivy::tokenizer::TextAnalyzer;
use tracing::{debug, info};

use crate::bitset::BitSet;
use crate::partition::Partition;
use crate::synonyms::Synonyms;
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
#[derive(Clone)]
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

/// Full geometry of a hit.
pub enum Shape {
    Point(LonLat),
    Lines(Vec<Vec<LonLat>>),
    Polygons(Vec<geors_core::storage::PolygonRings>),
}

impl Hit {
    /// Load the stored line / polygon geometry (or the centre point).
    pub fn shape(&self) -> Shape {
        let Some(rec) = self.partition.record(self.place_id) else {
            return Shape::Point(self.place.center);
        };
        let lines = self.partition.lines(&rec);
        if !lines.is_empty() {
            return Shape::Lines(lines);
        }
        let polygons = self.partition.polygons(&rec);
        if !polygons.is_empty() {
            return Shape::Polygons(polygons);
        }
        Shape::Point(self.place.center)
    }

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

/// Query-time settings.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub ranking: RankingConfig,
    pub synonyms: Synonyms,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            ranking: RankingConfig::default(),
            synonyms: Synonyms::builtin(),
        }
    }
}

/// Text searches in progress, across engines (the server swaps engines on
/// reload).
static SEARCHES: AtomicUsize = AtomicUsize::new(0);

/// Counts a search in progress while alive.
struct InFlight;

impl InFlight {
    /// Returns the guard and the number of searches now in progress.
    fn enter() -> (Self, usize) {
        (Self, SEARCHES.fetch_add(1, AtomicOrdering::Relaxed) + 1)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        SEARCHES.fetch_sub(1, AtomicOrdering::Relaxed);
    }
}

pub struct Engine {
    partitions: Vec<Arc<Partition>>,
    ranking: RankingConfig,
    synonyms: Synonyms,
    analyzer: TextAnalyzer,
}

impl Engine {
    /// Open the partitions in `data_dir`. If `countries` is non-empty only
    /// those are loaded (and each must exist).
    pub fn open(
        data_dir: &Path,
        countries: &[String],
        config: EngineConfig,
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
        Ok(Self::from_partitions(partitions, config))
    }

    pub fn from_partitions(partitions: Vec<Arc<Partition>>, config: EngineConfig) -> Self {
        Self {
            partitions,
            ranking: config.ranking,
            synonyms: config.synonyms,
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

        // A query of only punctuation (e.g. "(" while typing) has no words:
        // nothing can match, which is not a client error.
        let no_words = req.query.as_deref().is_some_and(|q| !q.trim().is_empty());
        let Some(parsed) = parsed else {
            if no_words && filter_box.is_none() {
                return Ok(Vec::new());
            }
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
        // Partitions are independent, so one search can use all cores. That
        // cuts latency on a quiet server, but when many searches run at once
        // the cores are already busy and splitting each one only adds
        // overhead (measured on Europe, 55 partitions: 4x more req/s for a
        // single client, 20 % fewer with 32).
        let (_in_flight, running) = InFlight::enter();
        let parallel = parts.len() > 1 && running <= (rayon::current_num_threads() / 2).max(1);
        // Hard spatial filter per partition, computed once for all phases.
        // `None` = no filter; partitions with an empty candidate set are skipped.
        let mut filters: Vec<(usize, Option<Arc<BitSet>>)> = Vec::new();
        for &pi in &parts {
            match &filter_box {
                Some(fb) => {
                    let set = self.spatial_candidates(&self.partitions[pi], fb, req)?;
                    if !set.is_empty() {
                        filters.push((pi, Some(Arc::new(set))));
                    }
                }
                None => filters.push((pi, None)),
            }
        }
        // Cheapest first; a later phase only runs if the earlier ones found
        // nothing (a precise address legitimately has a single hit, so
        // "fewer than limit" would make every such query pay for fuzzy
        // matching): (fuzzy, words allowed to be missing, penalty).
        let phases = [(false, 0usize, 1.0f32), (true, 0, 1.0), (true, 1, 0.5)];
        let mut cands: Vec<Candidate> = Vec::new();
        for (phase, &(fuzzy, slack, penalty)) in phases.iter().enumerate() {
            if phase > 0 && !cands.is_empty() {
                break;
            }
            if slack > 0 && parsed.tokens.len() < 2 {
                break;
            }
            // Merged in partition order, so results do not depend on timing.
            // A partition returns each place at most once, and later phases
            // start from an empty list, so no duplicate check is needed.
            let search_part = |(pi, filter): &(usize, Option<Arc<BitSet>>)| -> Result<Vec<Candidate>, EngineError> {
                    let part = &self.partitions[*pi];
                    let query =
                        part.text
                            .build_query(&parsed, &req.layers, slack, fuzzy, &self.synonyms);
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
                        .search(query.as_ref(), filter.clone(), scorer, fetch)
                        .map_err(IndexError::from)?;
                    Ok(hits
                        .into_iter()
                        .map(|(score, id)| Candidate {
                            part: *pi,
                            id,
                            score,
                            distance_m: None,
                        })
                        .collect())
                };
            let per_part: Vec<Vec<Candidate>> = if parallel {
                filters
                    .par_iter()
                    .map(search_part)
                    .collect::<Result<_, EngineError>>()?
            } else {
                filters
                    .iter()
                    .map(search_part)
                    .collect::<Result<_, EngineError>>()?
            };
            cands.extend(per_part.into_iter().flatten());
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
        let mut ids = Vec::new();
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
            ids.push(id);
        }
        Ok(BitSet::from_ids(ids, part.len() as usize))
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
        let max = req.max_distance_m.unwrap_or(f64::INFINITY);
        let passes = |layer: Layer| req.layers.is_empty() || req.layers.contains(&layer);
        // Nearest partitions first: once the k-th best distance is below a
        // partition's distance, it and all later ones can be skipped.
        let mut parts: Vec<(f64, usize)> = self
            .select(&req.countries)?
            .into_iter()
            .map(|pi| (lower_bound_m(&self.partitions[pi].meta.bbox, p), pi))
            .collect();
        parts.sort_by(|a, b| a.0.total_cmp(&b.0));
        // Best so far, ascending: (distance, partition, id).
        let mut best: Vec<(f64, usize, u32)> = Vec::with_capacity(limit + 1);
        let bound = |best: &Vec<(f64, usize, u32)>| {
            if best.len() == limit {
                best[limit - 1].0.min(max)
            } else {
                max
            }
        };
        // Seed the bound from every partition's first few candidates. Country
        // bboxes overlap along borders, so a partition whose box contains
        // the point may still have nothing nearby; without a seed its walk
        // would have to grow until it found `limit` places far away.
        for &(_, pi) in &parts {
            let part = &self.partitions[pi];
            for (id, _) in part.neighbors_lower_bound(p, limit * 2, bound(&best))? {
                let Some(rec) = part.record(id) else { continue };
                if !passes(rec.layer) || best.iter().any(|b| b.1 == pi && b.2 == id) {
                    continue;
                }
                let d = part.distance(&rec, p);
                if d <= bound(&best) {
                    let at = best.partition_point(|b| b.0 <= d);
                    best.insert(at, (d, pi, id));
                    best.truncate(limit);
                }
            }
        }
        for (lower, pi) in parts {
            if lower > bound(&best) {
                continue;
            }
            let part = &self.partitions[pi];
            // Best-first k-NN: walk places in order of a lower bound on their
            // distance; stop once the bound exceeds the k-th exact distance.
            // (Long rivers have huge bboxes, so bbox order alone is no good.)
            let mut want = (limit * 4).max(32);
            let (mut visited, mut rounds) = (0usize, 0usize);
            loop {
                rounds += 1;
                let list = part.neighbors_lower_bound(p, want, bound(&best))?;
                let mut done = list.len() < want;
                for &(id, lower) in &list {
                    if lower > bound(&best) {
                        done = true;
                        break;
                    }
                    if best.iter().any(|b| b.1 == pi && b.2 == id) {
                        continue;
                    }
                    let Some(rec) = part.record(id) else { continue };
                    if !passes(rec.layer) {
                        continue;
                    }
                    visited += 1;
                    let d = part.distance(&rec, p);
                    if d <= bound(&best) {
                        let at = best.partition_point(|b| b.0 <= d);
                        best.insert(at, (d, pi, id));
                        best.truncate(limit);
                    }
                }
                if done {
                    break;
                }
                want *= 4;
            }
            debug!(partition = part.code(), visited, rounds, want, "nearest");
        }
        let cands = best
            .into_iter()
            .map(|(d, part, id)| Candidate {
                part,
                id,
                score: self.ranking.proximity(d),
                distance_m: Some(d),
            })
            .collect();
        self.load(cands, Some(p))
    }

    /// Places by OSM id, in request order. Unknown ids are skipped.
    /// Merged street segments are found by any of their way ids.
    pub fn lookup(
        &self,
        ids: &[(OsmType, i64)],
        countries: &[String],
    ) -> Result<Vec<Hit>, EngineError> {
        let keys: Vec<String> = ids.iter().map(|(t, id)| text::osm_key(*t, *id)).collect();
        let mut found = Vec::new();
        for pi in self.select(countries)? {
            let part = &self.partitions[pi];
            let cands: Vec<Candidate> = part
                .text
                .lookup(&keys)
                .map_err(IndexError::from)?
                .into_iter()
                .map(|id| Candidate {
                    part: pi,
                    id,
                    score: 1.0,
                    distance_m: None,
                })
                .collect();
            found.extend(self.load(cands, None)?);
        }
        let matches = |h: &Hit, t: OsmType, id: i64| {
            h.place.osm_type == t && (h.place.osm_id == id || h.place.merged_ids.contains(&id))
        };
        Ok(ids
            .iter()
            .filter_map(|&(t, id)| found.iter().find(|h| matches(h, t, id)).cloned())
            .collect())
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
                Some(part.doc(&rec, false).map(|place| Hit {
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

/// A conservative lower bound (metres) on the distance from `p` to any
/// point in `b`: haversine to the clamped point, reduced by 10 % because
/// on a sphere the clamped point is not always the closest one.
fn lower_bound_m(b: &BBox, p: LonLat) -> f64 {
    let c = LonLat::new(
        p.lon.clamp(b.min_lon, b.max_lon),
        p.lat.clamp(b.min_lat, b.max_lat),
    );
    geors_core::geom::haversine(p, c) * 0.9
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
