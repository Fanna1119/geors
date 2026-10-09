//! Tantivy full-text index: schema, analysis and query construction.
//!
//! The text index stores nothing but postings and a `place_id` fast field.
//! Everything needed to render or rank a hit lives in the partition's own
//! record/doc files, which keeps the index small.

use std::path::Path;
use std::sync::Arc;

use geors_core::{Layer, Place};
use tantivy::collector::{FilterCollector, TopDocs};
use tantivy::query::{
    BooleanQuery, BoostQuery, ConstScoreQuery, DisjunctionMaxQuery, FuzzyTermQuery, Occur, Query,
    TermQuery,
};
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, STRING, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
};
use tantivy::{
    DocId, Index, IndexReader, IndexWriter, ReloadPolicy, Score, SegmentReader, TantivyDocument,
    Term,
};

use crate::bitset::BitSet;

pub const TOKENIZER: &str = "geo";

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
}

pub fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let text = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqs),
    );
    let fields = Fields {
        place_id: b.add_u64_field("place_id", NumericOptions::default().set_fast()),
        name: b.add_text_field("name", text.clone()),
        alt_names: b.add_text_field("alt_names", text.clone()),
        address: b.add_text_field("address", text.clone()),
        housenumber: b.add_text_field("housenumber", text),
        layer: b.add_text_field("layer", STRING),
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

pub fn tokenize(analyzer: &mut TextAnalyzer, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stream = analyzer.token_stream(text);
    stream.process(&mut |t| out.push(t.text.clone()));
    out
}

/// Text index writer used during import.
pub struct TextIndexWriter {
    writer: IndexWriter,
    fields: Fields,
}

impl TextIndexWriter {
    pub fn create(dir: &Path) -> tantivy::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (schema, fields) = schema();
        let index = Index::create_in_dir(dir, schema)?;
        index.tokenizers().register(TOKENIZER, analyzer());
        // A single thread keeps the import's memory footprint predictable.
        let writer = index.writer_with_num_threads(1, 64 * 1024 * 1024)?;
        Ok(Self { writer, fields })
    }

    /// Index a place. `context` holds the names of its parents (city, state, ...).
    pub fn add(&mut self, place_id: u32, place: &Place, context: &[&str]) -> tantivy::Result<()> {
        let f = self.fields;
        let mut doc = TantivyDocument::default();
        doc.add_u64(f.place_id, place_id as u64);
        doc.add_text(f.layer, place.layer.as_str());
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

    pub fn finish(mut self) -> tantivy::Result<()> {
        self.writer.commit()?;
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

pub struct TextIndex {
    reader: IndexReader,
    fields: Fields,
}

impl TextIndex {
    pub fn open(dir: &Path) -> tantivy::Result<Self> {
        let index = Index::open_in_dir(dir)?;
        index.tokenizers().register(TOKENIZER, analyzer());
        let (_, fields) = schema();
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self { reader, fields })
    }

    /// One query word: the best matching field counts (dis-max), so a POI
    /// named "Vaduz" in the city of Vaduz does not score twice.
    fn token_clause(&self, token: &str, prefix: bool) -> Box<dyn Query> {
        let f = self.fields;
        let distance = fuzzy_distance(token);
        let per_field = [
            (f.name, NAME_BOOST, true),
            (f.alt_names, ALT_NAME_BOOST, true),
            (f.address, ADDRESS_BOOST, true),
            (f.housenumber, HOUSENUMBER_BOOST, false),
        ]
        .into_iter()
        .map(|(field, boost, fuzzy)| {
            let term = Term::from_field_text(field, token);
            let mut variants: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            let mut push = |q: Box<dyn Query>, b: f32| {
                variants.push((Occur::Should, Box::new(BoostQuery::new(q, b))));
            };
            push(
                Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs)),
                boost,
            );
            if prefix {
                push(
                    Box::new(FuzzyTermQuery::new_prefix(term.clone(), 0, true)),
                    boost * 0.6,
                );
                if fuzzy && distance > 0 {
                    push(
                        Box::new(FuzzyTermQuery::new_prefix(term, 1, true)),
                        boost * 0.3,
                    );
                }
            } else if fuzzy && distance > 0 {
                push(
                    Box::new(FuzzyTermQuery::new(term, distance, true)),
                    boost * 0.4,
                );
            }
            Box::new(BooleanQuery::new(variants)) as Box<dyn Query>
        })
        .collect();
        Box::new(DisjunctionMaxQuery::with_tie_breaker(per_field, 0.1))
    }

    /// Build the query: every token must match (or all but `slack` tokens),
    /// restricted to `layers` if non-empty.
    pub fn build_query(&self, q: &ParsedQuery, layers: &[Layer], slack: usize) -> Box<dyn Query> {
        let n = q.tokens.len();
        let clauses: Vec<Box<dyn Query>> = q
            .tokens
            .iter()
            .enumerate()
            .map(|(i, t)| self.token_clause(t, q.last_is_prefix && i + 1 == n))
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
    fn fuzziness() {
        assert_eq!(fuzzy_distance("35"), 0);
        assert_eq!(fuzzy_distance("rue"), 0);
        assert_eq!(fuzzy_distance("vaduz"), 1);
        assert_eq!(fuzzy_distance("liechtenstein"), 2);
    }
}
