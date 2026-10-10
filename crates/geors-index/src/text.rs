//! Tantivy full-text index: schema, analysis and query construction.
//!
//! The text index stores nothing but postings and a `place_id` fast field.
//! Everything needed to render or rank a hit lives in the partition's own
//! record/doc files, which keeps the index small.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use geors_core::{Layer, OsmType, Place};
use tantivy::collector::{FilterCollector, TopDocs};
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, Lock, MmapDirectory, WatchCallback, WatchHandle, WritePtr,
};
use tantivy::indexer::NoMergePolicy;
use tantivy::query::{
    BooleanQuery, BoostQuery, ConstScoreQuery, DisjunctionMaxQuery, FuzzyTermQuery, Occur, Query,
    TermQuery,
};
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, STRING, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
    WhitespaceTokenizer,
};
use tantivy::{
    DocId, Index, IndexReader, IndexWriter, ReloadPolicy, Score, SegmentReader, TantivyDocument,
    Term,
};

use crate::bitset::BitSet;
use crate::synonyms::Synonyms;

pub const TOKENIZER: &str = "geo";
/// Splits on whitespace only: the n-grams are produced (and normalised)
/// at index time.
pub const PREFIX_TOKENIZER: &str = "geo_prefix";

/// Longest indexed name prefix, in characters. Longer typed prefixes fall
/// back to a dictionary scan (rare: few words are that long).
const MAX_PREFIX: usize = 12;
/// Address prefixes (city names typed partially) only from this length on:
/// shorter ones match too many terms to be useful.
const MIN_ADDRESS_PREFIX: usize = 3;
/// Prefixes up to this length are answered from `major_prefix`.
const SHORT_PREFIX: usize = 2;
/// Importance from which a place counts as "major" (cities, towns,
/// villages, regions; POIs with a Wikipedia/Wikidata link).
const MAJOR_IMPORTANCE: f32 = 0.3;

const NAME_BOOST: f32 = 3.0;
const ALT_NAME_BOOST: f32 = 2.5;
const ADDRESS_BOOST: f32 = 1.0;
const HOUSENUMBER_BOOST: f32 = 1.5;

#[derive(Clone, Copy)]
pub struct Fields {
    pub place_id: Field,
    /// Primary name only, so that its length norm is not diluted.
    pub name: Field,
    /// Translations and alternative names.
    pub alt_names: Field,
    pub address: Field,
    pub housenumber: Field,
    pub layer: Field,
    /// `N123` / `W456` / `R789`, for lookup by OSM id.
    pub osm: Field,
    /// Edge n-grams of every name word ("hau", "haup", ...): a typed prefix
    /// is a single term lookup instead of a scan of the term dictionary.
    pub name_prefix: Field,
    /// One- and two-letter prefixes of important places only (cities,
    /// regions, well-known POIs): typing "m" suggests München, not one of
    /// millions of names starting with m.
    pub major_prefix: Field,
}

pub fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let text = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqs),
    );
    // No term frequencies and no length norms: a prefix either matches or
    // not, and ranking comes from importance and distance, not from how
    // short a name is ("mun mun" must not beat München for "mün").
    let prefix_options = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(PREFIX_TOKENIZER)
            .set_index_option(IndexRecordOption::Basic)
            .set_fieldnorms(false),
    );
    let fields = Fields {
        place_id: b.add_u64_field("place_id", NumericOptions::default().set_fast()),
        name: b.add_text_field("name", text.clone()),
        alt_names: b.add_text_field("alt_names", text.clone()),
        address: b.add_text_field("address", text.clone()),
        housenumber: b.add_text_field("housenumber", text),
        layer: b.add_text_field("layer", STRING),
        osm: b.add_text_field("osm", STRING),
        name_prefix: b.add_text_field("name_prefix", prefix_options.clone()),
        major_prefix: b.add_text_field("major_prefix", prefix_options),
    };
    (b.build(), fields)
}

/// Lowercasing, accent folding word tokenizer ("Städtle-Straße" -> stadtle, strasse).
pub fn analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

fn register_tokenizers(index: &Index) {
    index.tokenizers().register(TOKENIZER, analyzer());
    index.tokenizers().register(
        PREFIX_TOKENIZER,
        TextAnalyzer::builder(WhitespaceTokenizer::default()).build(),
    );
}

/// All prefixes (1..=MAX_PREFIX characters) of every token of `names`.
fn edge_ngrams<'a>(
    analyzer: &mut TextAnalyzer,
    names: impl Iterator<Item = &'a str>,
    max_len: usize,
) -> String {
    let mut grams = std::collections::BTreeSet::new();
    for name in names {
        for token in tokenize(analyzer, name) {
            for (n, (i, c)) in token.char_indices().enumerate() {
                if n >= max_len {
                    break;
                }
                grams.insert(token[..i + c.len_utf8()].to_string());
            }
        }
    }
    grams.into_iter().collect::<Vec<_>>().join(" ")
}

pub fn tokenize(analyzer: &mut TextAnalyzer, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stream = analyzer.token_stream(text);
    stream.process(&mut |t| out.push(t.text.clone()));
    out
}

/// Term used for OSM id lookup, e.g. `W123`.
pub fn osm_key(t: OsmType, id: i64) -> String {
    format!("{}{id}", t.as_str())
}

/// tantivy needs at least this much indexing memory per thread.
const MIN_THREAD_BUDGET: usize = 15 * 1024 * 1024;
const DEFAULT_THREAD_BUDGET: usize = 48 * 1024 * 1024;

/// Total indexing memory (0 = default: 48 MB per thread).
static INDEX_MEMORY: AtomicUsize = AtomicUsize::new(0);

/// Limit the memory the text indexer may use during import.
pub fn set_index_memory(bytes: usize) {
    INDEX_MEMORY.store(bytes.max(MIN_THREAD_BUDGET), Ordering::Relaxed);
}

/// Indexing threads (at most 4, at most the rayon pool) and total budget.
fn index_threads_and_budget() -> (usize, usize) {
    let threads = rayon::current_num_threads().clamp(1, 4);
    match INDEX_MEMORY.load(Ordering::Relaxed) {
        0 => (threads, threads * DEFAULT_THREAD_BUDGET),
        budget => {
            let threads = threads.min(budget / MIN_THREAD_BUDGET).max(1);
            (threads, budget)
        }
    }
}

/// Text index writer used during import.
pub struct TextIndexWriter {
    index: Index,
    writer: IndexWriter,
    fields: Fields,
    analyzer: TextAnalyzer,
}

impl TextIndexWriter {
    pub fn create(dir: &Path) -> tantivy::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (schema, fields) = schema();
        let index = Index::create_in_dir(dir, schema)?;
        register_tokenizers(&index);
        // A few indexing threads with a fixed budget keep memory predictable.
        let (threads, budget) = index_threads_and_budget();
        let writer: IndexWriter = index.writer_with_num_threads(threads, budget)?;
        // No background merges: `finish` merges everything into one segment
        // once, instead of merging the same data repeatedly.
        writer.set_merge_policy(Box::new(NoMergePolicy));
        Ok(Self {
            index,
            writer,
            fields,
            analyzer: analyzer(),
        })
    }

    /// Index a place. `context` holds the names of its parents (city, state, ...).
    pub fn add(&mut self, place_id: u32, place: &Place, context: &[&str]) -> tantivy::Result<()> {
        let f = self.fields;
        let mut doc = TantivyDocument::default();
        doc.add_u64(f.place_id, place_id as u64);
        doc.add_text(f.layer, place.layer.as_str());
        doc.add_text(f.osm, osm_key(place.osm_type, place.osm_id));
        for id in &place.merged_ids {
            doc.add_text(f.osm, osm_key(place.osm_type, *id));
        }
        // Unique names only: dozens of identical translations would inflate
        // the field length and depress the BM25 score.
        let mut seen = std::collections::HashSet::new();
        if let Some(name) = &place.name {
            seen.insert(name.to_lowercase());
            doc.add_text(f.name, name);
        }
        for name in place.all_names() {
            if seen.insert(name.to_lowercase()) {
                doc.add_text(f.alt_names, name);
            }
        }
        let grams = edge_ngrams(&mut self.analyzer, place.all_names(), MAX_PREFIX);
        if !grams.is_empty() {
            doc.add_text(f.name_prefix, grams);
        }
        if place.layer >= Layer::City || place.importance >= MAJOR_IMPORTANCE {
            let short = edge_ngrams(&mut self.analyzer, place.all_names(), SHORT_PREFIX);
            if !short.is_empty() {
                doc.add_text(f.major_prefix, short);
            }
        }
        for value in place
            .street
            .iter()
            .chain(place.postcode.iter())
            .chain(place.city.iter())
        {
            doc.add_text(f.address, value);
        }
        for value in context {
            doc.add_text(f.address, value);
        }
        if let Some(hn) = &place.housenumber {
            doc.add_text(f.housenumber, hn);
        }
        self.writer.add_document(doc)?;
        Ok(())
    }

    /// Commit and merge into a single segment: one term dictionary and one
    /// fast-field column per query instead of one per segment.
    pub fn finish(mut self) -> tantivy::Result<()> {
        self.writer.commit()?;
        let segments = self.index.searchable_segment_ids()?;
        if segments.len() > 1 {
            self.writer.merge(&segments).wait()?;
        }
        self.writer.wait_merging_threads()?;
        Ok(())
    }
}

/// A parsed free-text query.
#[derive(Debug, Clone)]
pub struct ParsedQuery {
    pub tokens: Vec<String>,
    /// The last token may be incomplete (search-as-you-type).
    pub last_is_prefix: bool,
}

impl ParsedQuery {
    pub fn parse(analyzer: &mut TextAnalyzer, q: &str, autocomplete: bool) -> Self {
        let tokens = tokenize(analyzer, q);
        // A trailing space means the user finished the last word.
        let last_is_prefix = autocomplete && !q.ends_with(char::is_whitespace);
        Self {
            tokens,
            last_is_prefix,
        }
    }
}

/// Edit distance allowed for a token: none for short words and numbers.
fn fuzzy_distance(token: &str) -> u8 {
    if token.chars().any(|c| c.is_ascii_digit()) {
        return 0;
    }
    match token.chars().count() {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// A finished partition's text index never changes (imports write a new
/// directory and swap it in), so readers need no lock files and nothing is
/// ever written. This lets geors serve from read-only volumes and images.
#[derive(Clone, Debug)]
struct ReadOnlyDirectory(MmapDirectory);

fn read_only(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("text index is read-only: {}", path.display()),
    )
}

impl Directory for ReadOnlyDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.0.get_file_handle(path)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(read_only(path)),
            filepath: path.to_path_buf(),
        })
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.0.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::wrap_io_error(
            read_only(path),
            path.to_path_buf(),
        ))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.0.atomic_read(path)
    }

    fn atomic_write(&self, path: &Path, _data: &[u8]) -> std::io::Result<()> {
        Err(read_only(path))
    }

    fn sync_directory(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn acquire_lock(&self, _lock: &Lock) -> Result<DirectoryLock, LockError> {
        Ok(DirectoryLock::from(Box::new(())))
    }

    fn watch(&self, _callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        Ok(WatchHandle::empty())
    }
}

pub struct TextIndex {
    reader: IndexReader,
    fields: Fields,
}

impl TextIndex {
    pub fn open(dir: &Path) -> tantivy::Result<Self> {
        let index = Index::open(ReadOnlyDirectory(MmapDirectory::open(dir)?))?;
        register_tokenizers(&index);
        let (_, fields) = schema();
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self { reader, fields })
    }

    /// One query word: the best matching field counts (dis-max), so a POI
    /// named "Vaduz" in the city of Vaduz does not score twice.
    ///
    /// Abbreviations ("str" -> "strasse") are added as exact alternatives.
    ///
    /// With `fuzzy`, misspellings within the allowed edit distance match too
    /// (much more expensive: the engine only does this when exact and prefix
    /// matching found too little).
    ///
    /// `context` says other words are present: only then may the last word
    /// prefix-match address context (city, country). A lone word is
    /// assumed to be a name; matching "deutsc" against the "Deutschland" in
    /// every German place's context would touch the whole partition.
    fn token_clause(
        &self,
        token: &str,
        prefix: bool,
        context: bool,
        fuzzy: bool,
        synonyms: &Synonyms,
    ) -> Box<dyn Query> {
        let f = self.fields;
        let distance = if fuzzy { fuzzy_distance(token) } else { 0 };
        let expansions = synonyms.expand(token);
        let chars = token.chars().count();
        let numeric = token.chars().any(|c| c.is_ascii_digit());
        let mut per_field: Vec<Box<dyn Query>> = Vec::new();
        for (field, boost) in [
            (f.name, NAME_BOOST),
            (f.alt_names, ALT_NAME_BOOST),
            (f.address, ADDRESS_BOOST),
            (f.housenumber, HOUSENUMBER_BOOST),
        ] {
            let mut variants: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            let mut push = |q: Box<dyn Query>, b: f32| {
                variants.push((Occur::Should, Box::new(BoostQuery::new(q, b))));
            };
            let term = Term::from_field_text(field, token);
            push(
                Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs)),
                boost,
            );
            for exp in &expansions {
                let t = Term::from_field_text(field, exp);
                push(
                    Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs)),
                    boost * 0.9,
                );
            }
            let is_name = field == f.name;
            if prefix && is_name {
                // Names and alternative names share the n-gram field.
                if chars <= SHORT_PREFIX {
                    let t = Term::from_field_text(f.major_prefix, token);
                    push(
                        Box::new(TermQuery::new(t, IndexRecordOption::Basic)),
                        boost * 0.6,
                    );
                } else if chars <= MAX_PREFIX {
                    let t = Term::from_field_text(f.name_prefix, token);
                    push(
                        Box::new(TermQuery::new(t, IndexRecordOption::Basic)),
                        boost * 0.6,
                    );
                } else {
                    push(
                        Box::new(FuzzyTermQuery::new_prefix(term.clone(), 0, true)),
                        boost * 0.6,
                    );
                }
            }
            if prefix && context && field == f.address && chars >= MIN_ADDRESS_PREFIX && !numeric {
                push(
                    Box::new(FuzzyTermQuery::new_prefix(term.clone(), 0, true)),
                    boost * 0.6,
                );
            }
            // Typos: only names and address, never house numbers.
            if distance > 0 && (is_name || field == f.address) {
                let q = if prefix {
                    FuzzyTermQuery::new_prefix(term, 1, true)
                } else {
                    FuzzyTermQuery::new(term, distance, true)
                };
                push(Box::new(q), boost * if prefix { 0.3 } else { 0.4 });
            }
            per_field.push(Box::new(BooleanQuery::new(variants)));
        }
        Box::new(DisjunctionMaxQuery::with_tie_breaker(per_field, 0.1))
    }

    /// Build the query: every token must match (or all but `slack` tokens),
    /// restricted to `layers` if non-empty.
    pub fn build_query(
        &self,
        q: &ParsedQuery,
        layers: &[Layer],
        slack: usize,
        fuzzy: bool,
        synonyms: &Synonyms,
    ) -> Box<dyn Query> {
        let n = q.tokens.len();
        let clauses: Vec<Box<dyn Query>> = q
            .tokens
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let prefix = q.last_is_prefix && i + 1 == n;
                // Words that exist as typed need no typo expansion (the
                // expensive part): only fuzz the unknown ones.
                let fuzzy = fuzzy && (prefix || !self.is_known(t));
                self.token_clause(t, prefix, n > 1, fuzzy, synonyms)
            })
            .collect();
        let required = n.saturating_sub(slack).max(1);
        let text: Box<dyn Query> = if required == n {
            Box::new(BooleanQuery::intersection(clauses))
        } else {
            Box::new(BooleanQuery::union_with_minimum_required_clauses(
                clauses, required,
            ))
        };
        if layers.is_empty() {
            return text;
        }
        let layer_filter = BooleanQuery::new_multiterms_query(
            layers
                .iter()
                .map(|l| Term::from_field_text(self.fields.layer, l.as_str()))
                .collect(),
        );
        Box::new(BooleanQuery::new(vec![
            (Occur::Must, text),
            (
                Occur::Must,
                Box::new(ConstScoreQuery::new(Box::new(layer_filter), 0.0)),
            ),
        ]))
    }

    /// Whether `token` occurs as a word in any name or address.
    fn is_known(&self, token: &str) -> bool {
        let searcher = self.reader.searcher();
        let f = self.fields;
        [f.name, f.alt_names, f.address].into_iter().any(|field| {
            searcher
                .doc_freq(&Term::from_field_text(field, token))
                .is_ok_and(|n| n > 0)
        })
    }

    /// Place ids indexed under any of the given OSM keys (see [`osm_key`]).
    pub fn lookup(&self, keys: &[String]) -> tantivy::Result<Vec<u32>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let query = BooleanQuery::new_multiterms_query(
            keys.iter()
                .map(|k| Term::from_field_text(self.fields.osm, k))
                .collect(),
        );
        let scorer = Arc::new(|_: u32, s: Score| s);
        Ok(self
            .search(&query, None, scorer, keys.len() * 4)?
            .into_iter()
            .map(|(_, id)| id)
            .collect())
    }

    /// Run `query`, keeping only place ids in `filter` (if any), ranking by
    /// `scorer(place_id, text_score)`. Returns `(score, place_id)` best first.
    pub fn search(
        &self,
        query: &dyn Query,
        filter: Option<Arc<BitSet>>,
        scorer: Arc<dyn Fn(u32, Score) -> Score + Send + Sync>,
        limit: usize,
    ) -> tantivy::Result<Vec<(Score, u32)>> {
        let searcher = self.reader.searcher();
        let top = TopDocs::with_limit(limit.max(1)).tweak_score(move |segment: &SegmentReader| {
            let ids = segment
                .fast_fields()
                .u64("place_id")
                .expect("place_id fast field")
                .first_or_default_col(0);
            let scorer = scorer.clone();
            move |doc: DocId, score: Score| {
                let id = ids.get_val(doc) as u32;
                (scorer(id, score), id)
            }
        });
        let collector = FilterCollector::new(
            "place_id".to_string(),
            move |id: u64| filter.as_ref().is_none_or(|f| f.contains(id as u32)),
            top,
        );
        let hits = searcher.search(query, &collector)?;
        Ok(hits
            .into_iter()
            .map(|((score, id), _)| (score, id))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analysis_folds_and_splits() {
        let mut a = analyzer();
        assert_eq!(
            tokenize(&mut a, "Städtle-Straße 35a"),
            vec!["stadtle", "strasse", "35a"]
        );
    }

    #[test]
    fn prefix_detection() {
        let mut a = analyzer();
        assert!(ParsedQuery::parse(&mut a, "vadu", true).last_is_prefix);
        assert!(!ParsedQuery::parse(&mut a, "vaduz ", true).last_is_prefix);
        assert!(!ParsedQuery::parse(&mut a, "vadu", false).last_is_prefix);
    }

    #[test]
    fn ngrams() {
        let mut a = analyzer();
        let g = edge_ngrams(&mut a, ["Städtle 35", "Ab"].into_iter(), MAX_PREFIX);
        assert_eq!(g, "3 35 a ab s st sta stad stadt stadtl stadtle");
        let long = edge_ngrams(&mut a, ["Donaudampfschifffahrt"].into_iter(), MAX_PREFIX);
        assert_eq!(
            long.split(' ').map(|t| t.chars().count()).max(),
            Some(MAX_PREFIX)
        );
    }

    #[test]
    fn fuzziness() {
        assert_eq!(fuzzy_distance("35"), 0);
        assert_eq!(fuzzy_distance("rue"), 0);
        assert_eq!(fuzzy_distance("vaduz"), 1);
        assert_eq!(fuzzy_distance("liechtenstein"), 2);
    }
}
