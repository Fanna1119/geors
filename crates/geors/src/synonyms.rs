//! Abbreviation and synonym rules (`str` -> `strasse`, `st` -> `saint`,
//! ...): built-in rule files per language, plus rules from a config.

pub use geors_index::synonyms::{SynonymConfig, Synonyms, builtin_languages};

/// The words of `text` as the search index sees them: lowercased, accents
/// folded, split on punctuation (`"Städtle-Straße"` -> `stadtle`, `strasse`).
pub fn normalize(text: &str) -> Vec<String> {
    let mut analyzer = geors_index::text::analyzer();
    geors_index::text::tokenize(&mut analyzer, text)
}
