//! Query-time abbreviation and synonym expansion ("str" -> "strasse").
//!
//! Rules work on normalised tokens (lowercase, accents folded). A rule key
//! starting with `*` is a suffix rule: `*str = ["strasse"]` turns
//! "hauptstr" into "hauptstrasse".

use std::collections::{BTreeMap, HashMap};

use tantivy::tokenizer::TextAnalyzer;
use tracing::warn;

use crate::text;

const WORDS: &[(&str, &[&str])] = &[
    ("str", &["strasse"]),
    ("st", &["sankt", "saint", "street"]),
    ("ste", &["sainte"]),
    ("pl", &["platz", "place", "plaza"]),
    ("av", &["avenue", "avinguda", "avenida"]),
    ("ave", &["avenue"]),
    ("avda", &["avenida", "avinguda"]),
    ("bd", &["boulevard"]),
    ("blvd", &["boulevard"]),
    ("rd", &["road"]),
    ("dr", &["drive", "doktor", "doctor"]),
    ("ln", &["lane"]),
    ("sq", &["square"]),
    ("mt", &["mount", "mont", "monte"]),
    ("hbf", &["hauptbahnhof"]),
    ("bhf", &["bahnhof"]),
    ("pza", &["plaza", "piazza"]),
    ("rte", &["route"]),
    ("ch", &["chemin"]),
    ("stn", &["station"]),
    ("ctra", &["carretera"]),
    ("hl", &["heilige", "heiligen"]),
];

const SUFFIXES: &[(&str, &[&str])] = &[("str", &["strasse"]), ("pl", &["platz"])];

#[derive(Debug, Clone, Default)]
pub struct Synonyms {
    words: HashMap<String, Vec<String>>,
    /// (suffix, replacements), longest suffix first.
    suffixes: Vec<(String, Vec<String>)>,
}

impl Synonyms {
    /// Built-in rules plus `extra` from configuration (keys and values are
    /// normalised with the index analyzer; multi-word values are ignored).
    pub fn new(extra: &BTreeMap<String, Vec<String>>) -> Self {
        let mut analyzer = text::analyzer();
        let mut s = Self::default();
        for (k, vs) in WORDS {
            s.add(&mut analyzer, k, vs.iter().copied());
        }
        for (k, vs) in SUFFIXES {
            s.add(&mut analyzer, &format!("*{k}"), vs.iter().copied());
        }
        for (k, vs) in extra {
            s.add(&mut analyzer, k, vs.iter().map(String::as_str));
        }
        s.suffixes.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
        s
    }

    fn normalize(analyzer: &mut TextAnalyzer, word: &str) -> Option<String> {
        match text::tokenize(analyzer, word).as_slice() {
            [one] => Some(one.clone()),
            _ => {
                warn!(word, "ignoring synonym that is not a single word");
                None
            }
        }
    }

    fn add<'a>(
        &mut self,
        analyzer: &mut TextAnalyzer,
        key: &str,
        values: impl Iterator<Item = &'a str>,
    ) {
        let (suffix, key) = match key.strip_prefix('*') {
            Some(k) => (true, k),
            None => (false, key),
        };
        let Some(key) = Self::normalize(analyzer, key) else {
            return;
        };
        let values: Vec<String> = values
            .filter_map(|v| Self::normalize(analyzer, v))
            .collect();
        let slot = if suffix {
            match self.suffixes.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => v,
                None => {
                    self.suffixes.push((key, Vec::new()));
                    &mut self.suffixes.last_mut().unwrap().1
                }
            }
        } else {
            self.words.entry(key).or_default()
        };
        for v in values {
            if !slot.contains(&v) {
                slot.push(v);
            }
        }
    }

    /// The built-in rules only.
    pub fn builtin() -> Self {
        Self::new(&BTreeMap::new())
    }

    /// Alternatives for a normalised token (never includes the token itself).
    pub fn expand(&self, token: &str) -> Vec<String> {
        let mut out: Vec<String> = self.words.get(token).cloned().unwrap_or_default();
        for (suffix, repl) in &self.suffixes {
            if token.len() > suffix.len() + 1 && token.ends_with(suffix.as_str()) {
                let stem = &token[..token.len() - suffix.len()];
                out.extend(repl.iter().map(|r| format!("{stem}{r}")));
                break;
            }
        }
        out.retain(|t| t != token);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_and_custom_rules() {
        let mut extra = BTreeMap::new();
        extra.insert("Gr.".to_string(), vec!["Groß".to_string()]);
        extra.insert("*gs".to_string(), vec!["gasse".to_string()]);
        let s = Synonyms::new(&extra);
        assert_eq!(s.expand("str"), vec!["strasse"]);
        assert_eq!(s.expand("hauptstr"), vec!["hauptstrasse"]);
        assert_eq!(s.expand("marktpl"), vec!["marktplatz"]);
        assert_eq!(s.expand("gr"), vec!["gross"]);
        assert_eq!(s.expand("kirchgs"), vec!["kirchgasse"]);
        assert!(s.expand("vaduz").is_empty());
        // "str" itself is too short to be treated as a suffix.
        assert!(!s.expand("st").is_empty());
    }
}
