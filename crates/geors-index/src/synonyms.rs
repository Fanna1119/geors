//! Query-time abbreviation and synonym expansion ("str" -> "strasse").
//!
//! The rules are data, not code: one TOML file per language in
//! `<workspace>/synonyms/` (embedded at build time, see `build.rs`), plus
//! optional rule files and inline rules from the server configuration.
//! See `synonyms/README.md` for the file format.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tantivy::tokenizer::TextAnalyzer;

use crate::text;

mod builtin {
    include!(concat!(env!("OUT_DIR"), "/builtin_synonyms.rs"));
}

/// One rule file (or the inline rules of the configuration).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    /// Countries (ISO codes) whose partitions use these rules; empty = all.
    #[serde(default)]
    pub countries: Vec<String>,
    /// Whole words: typed word -> what it may stand for (a word or a
    /// phrase: `olv = ["onze lieve vrouw"]`).
    #[serde(default)]
    pub words: BTreeMap<String, Vec<String>>,
    /// Word endings: "hauptstr" -> "hauptstrasse" for `str = ["strasse"]`.
    #[serde(default)]
    pub suffixes: BTreeMap<String, Vec<String>>,
}

/// Which rules to load (the `[synonyms]` section of the server config).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SynonymConfig {
    /// Built-in languages to use (e.g. `["de", "en"]`); empty = all.
    pub languages: Vec<String>,
    /// Directory with additional rule files (`*.toml`, same format).
    pub dir: Option<PathBuf>,
    /// Inline rules (`[synonyms.words]`, `[synonyms.suffixes]`); they apply
    /// to all countries.
    pub words: BTreeMap<String, Vec<String>>,
    pub suffixes: BTreeMap<String, Vec<String>>,
    /// Apply every rule file to every country, ignoring their `countries`
    /// (the behaviour before rule files could name countries).
    pub everywhere: bool,
}

#[derive(Debug, Clone, Default)]
struct Rules {
    /// Word -> alternatives; a phrase alternative is space-separated.
    words: HashMap<String, Vec<String>>,
    /// (suffix, replacements), longest suffix first.
    suffixes: Vec<(String, Vec<String>)>,
}

impl Rules {
    fn is_empty(&self) -> bool {
        self.words.is_empty() && self.suffixes.is_empty()
    }

    fn expand_into(&self, token: &str, out: &mut Vec<String>) {
        if let Some(alts) = self.words.get(token) {
            merge_into(out, alts.iter().cloned());
        }
        for (suffix, repl) in &self.suffixes {
            if token.len() > suffix.len() + 1 && token.ends_with(suffix.as_str()) {
                let stem = &token[..token.len() - suffix.len()];
                merge_into(out, repl.iter().map(|r| format!("{stem}{r}")));
                break;
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Synonyms {
    /// Rules for every country.
    global: Rules,
    /// Rules limited to some countries, by lowercase country code.
    by_country: HashMap<String, Rules>,
}

/// Languages with built-in rule files.
pub fn builtin_languages() -> Vec<&'static str> {
    builtin::BUILTIN.iter().map(|(lang, _)| *lang).collect()
}

fn normalize(analyzer: &mut TextAnalyzer, word: &str) -> Result<String, String> {
    match text::tokenize(analyzer, word).as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("'{word}' has no letters or digits")),
        _ => Err(format!("'{word}' is not a single word")),
    }
}

impl Synonyms {
    /// All built-in rules.
    pub fn builtin() -> Self {
        Self::load(&SynonymConfig::default()).expect("built-in synonym files are valid")
    }

    /// Built-in rules (optionally limited to some languages), rule files
    /// from `dir`, and inline rules. Errors name the offending file.
    pub fn load(config: &SynonymConfig) -> Result<Self, String> {
        let mut analyzer = text::analyzer();
        let mut s = Self::default();
        let everywhere = config.everywhere;
        for lang in &config.languages {
            if !builtin_languages().contains(&lang.as_str()) {
                return Err(format!(
                    "no built-in synonyms for language '{lang}' (available: {})",
                    builtin_languages().join(", ")
                ));
            }
        }
        for (lang, text) in builtin::BUILTIN {
            if config.languages.is_empty() || config.languages.iter().any(|l| l == lang) {
                s.add_file(
                    &mut analyzer,
                    &format!("built-in {lang}.toml"),
                    text,
                    everywhere,
                )?;
            }
        }
        if let Some(dir) = &config.dir {
            for path in rule_files(dir)? {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                s.add_file(
                    &mut analyzer,
                    &path.display().to_string(),
                    &text,
                    everywhere,
                )?;
            }
        }
        let inline = RuleSet {
            countries: Vec::new(),
            words: config.words.clone(),
            suffixes: config.suffixes.clone(),
        };
        s.add_rules(&mut analyzer, "[synonyms] in the config", &inline, true)?;
        for rules in std::iter::once(&mut s.global).chain(s.by_country.values_mut()) {
            rules
                .suffixes
                .sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
        }
        Ok(s)
    }

    fn add_file(
        &mut self,
        analyzer: &mut TextAnalyzer,
        name: &str,
        text: &str,
        everywhere: bool,
    ) -> Result<(), String> {
        let rules: RuleSet = toml::from_str(text).map_err(|e| format!("{name}: {e}"))?;
        self.add_rules(analyzer, name, &rules, everywhere)
    }

    /// `everywhere`: ignore the rule set's `countries`.
    fn add_rules(
        &mut self,
        analyzer: &mut TextAnalyzer,
        name: &str,
        rules: &RuleSet,
        everywhere: bool,
    ) -> Result<(), String> {
        let mut targets: Vec<&mut Rules> = Vec::new();
        if everywhere || rules.countries.is_empty() {
            targets.push(&mut self.global);
        } else {
            let mut codes = Vec::new();
            for cc in &rules.countries {
                let code = geors_core::normalize_country_code(cc)
                    .ok_or_else(|| format!("{name}: '{cc}' is not a two-letter country code"))?;
                codes.push(code);
            }
            for code in codes {
                self.by_country.entry(code).or_default();
            }
            // Distinct entries, so they can be borrowed together.
            for (code, rules_for) in self.by_country.iter_mut() {
                if rules.countries.iter().any(|c| c.eq_ignore_ascii_case(code)) {
                    targets.push(rules_for);
                }
            }
        }
        let key = |analyzer: &mut TextAnalyzer, k: &str| {
            normalize(analyzer, k).map_err(|e| format!("{name}: key {e}"))
        };
        for (k, values) in &rules.words {
            let k = key(analyzer, k)?;
            let values = values
                .iter()
                .map(|v| {
                    normalize_phrase(analyzer, v)
                        .map_err(|e| format!("{name}: value {e} (for '{k}')"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            for t in targets.iter_mut() {
                merge_into(
                    t.words.entry(k.clone()).or_default(),
                    values.iter().cloned(),
                );
            }
        }
        for (k, values) in &rules.suffixes {
            let k = key(analyzer, k)?;
            let values = values
                .iter()
                .map(|v| {
                    normalize(analyzer, v)
                        .map_err(|e| format!("{name}: suffix value {e} (for '{k}')"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            for t in targets.iter_mut() {
                let slot = match t.suffixes.iter().position(|(s, _)| *s == k) {
                    Some(i) => &mut t.suffixes[i].1,
                    None => {
                        t.suffixes.push((k.clone(), Vec::new()));
                        &mut t.suffixes.last_mut().unwrap().1
                    }
                };
                merge_into(slot, values.iter().cloned());
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.by_country.values().all(Rules::is_empty)
    }

    /// Alternatives for a normalised token in the partition of `country`
    /// (lowercase ISO code; `None`: rules of every country). A phrase
    /// alternative is space-separated. Never includes the token itself.
    pub fn expand(&self, token: &str, country: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        self.global.expand_into(token, &mut out);
        match country {
            Some(cc) => {
                if let Some(rules) = self.by_country.get(cc) {
                    rules.expand_into(token, &mut out);
                }
            }
            None => {
                let mut codes: Vec<&String> = self.by_country.keys().collect();
                codes.sort();
                for cc in codes {
                    self.by_country[cc].expand_into(token, &mut out);
                }
            }
        }
        out.retain(|t| t != token);
        out
    }

    /// Countries with rules of their own (sorted).
    pub fn countries(&self) -> Vec<&str> {
        let mut codes: Vec<&str> = self.by_country.keys().map(String::as_str).collect();
        codes.sort_unstable();
        codes
    }
}

/// A rule value: one or more words, normalised and space-separated.
fn normalize_phrase(analyzer: &mut TextAnalyzer, value: &str) -> Result<String, String> {
    let words = text::tokenize(analyzer, value);
    if words.is_empty() {
        return Err(format!("'{value}' has no letters or digits"));
    }
    Ok(words.join(" "))
}

fn merge_into(slot: &mut Vec<String>, values: impl IntoIterator<Item = String>) {
    for v in values {
        if !slot.contains(&v) {
            slot.push(v);
        }
    }
}

/// `*.toml` files in `dir`, sorted (deterministic rule order).
fn rule_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read synonyms dir {}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shipped rule file must parse: valid country codes, single-word
    /// keys and suffix values.
    #[test]
    fn builtin_files_are_valid() {
        assert!(builtin_languages().len() >= 5);
        for (lang, text) in builtin::BUILTIN {
            let mut s = Synonyms::default();
            s.add_file(&mut text::analyzer(), lang, text, false)
                .unwrap_or_else(|e| panic!("synonyms/{lang}.toml: {e}"));
            assert!(!s.is_empty(), "synonyms/{lang}.toml has no rules");
        }
    }

    #[test]
    fn builtin_rules_across_languages() {
        let s = Synonyms::builtin();
        let hauptstr = s.expand("hauptstr", None);
        assert!(hauptstr.contains(&"hauptstrasse".to_string()));
        assert!(hauptstr.contains(&"hauptstraat".to_string()));
        assert!(s.expand("st", None).contains(&"street".to_string()));
        assert!(s.expand("st", None).contains(&"sankt".to_string()));
        assert_eq!(s.expand("cres", None), vec!["crescent"]);
        assert!(s.expand("vaduz", None).is_empty());
    }

    #[test]
    fn rules_apply_to_their_countries() {
        let s = Synonyms::builtin();
        // German rules in German-speaking partitions, English everywhere.
        let st_de = s.expand("st", Some("de"));
        assert!(st_de.contains(&"sankt".to_string()), "{st_de:?}");
        assert!(st_de.contains(&"street".to_string()), "{st_de:?}");
        let st_za = s.expand("st", Some("za"));
        assert!(!st_za.contains(&"sankt".to_string()), "{st_za:?}");
        assert!(
            s.expand("hauptstr", Some("za"))
                .iter()
                .all(|e| e != "hauptstrasse")
        );
        assert!(
            s.expand("hauptstr", Some("at"))
                .contains(&"hauptstrasse".to_string())
        );
        assert!(s.countries().contains(&"de"));
        // `everywhere`: the old behaviour.
        let all = Synonyms::load(&SynonymConfig {
            everywhere: true,
            ..Default::default()
        })
        .unwrap();
        assert!(all.expand("st", Some("za")).contains(&"sankt".to_string()));
        assert!(all.countries().is_empty());
    }

    #[test]
    fn phrase_values() {
        let s = Synonyms::builtin();
        assert_eq!(s.expand("olv", Some("be")), vec!["onze lieve vrouw"]);
        assert!(s.expand("olv", Some("de")).is_empty());
        assert_eq!(s.expand("nyc", None), vec!["new york city"]);
    }

    #[test]
    fn languages_dir_and_inline_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("local.toml"),
            "[words]\nkh = [\"krankenhaus\"]\n",
        )
        .unwrap();
        let mut config = SynonymConfig {
            languages: vec!["de".into()],
            dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        config.words.insert("Gr.".into(), vec!["Groß".into()]);
        config.suffixes.insert("gs".into(), vec!["gasse".into()]);
        let s = Synonyms::load(&config).unwrap();
        // Only German built-ins: no English "street".
        assert_eq!(s.expand("st", Some("de")), vec!["sankt"]);
        assert!(s.expand("st", Some("za")).is_empty());
        // Files without `countries` and inline rules apply everywhere.
        assert_eq!(s.expand("kh", Some("za")), vec!["krankenhaus"]);
        assert_eq!(s.expand("kirchgs", Some("za")), vec!["kirchgasse"]);
        assert!(s.expand("gr", Some("de")).contains(&"gross".to_string()));
    }

    #[test]
    fn bad_rules_are_reported() {
        let bad = |cfg: SynonymConfig| Synonyms::load(&cfg).unwrap_err();
        let e = bad(SynonymConfig {
            languages: vec!["xx".into()],
            ..Default::default()
        });
        assert!(e.contains("available"), "{e}");
        let mut cfg = SynonymConfig::default();
        cfg.words.insert("rue de la".into(), vec!["rue".into()]);
        assert!(bad(cfg).contains("not a single word"));
        let mut cfg = SynonymConfig::default();
        cfg.suffixes.insert("str".into(), vec!["stra sse".into()]);
        assert!(bad(cfg).contains("not a single word"));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("y.toml"),
            "countries = [\"xyz\"]\n[words]\na = [\"b\"]\n",
        )
        .unwrap();
        let e = bad(SynonymConfig {
            dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        });
        assert!(e.contains("country code"), "{e}");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.toml"), "[wrods]\na = [\"b\"]\n").unwrap();
        let e = bad(SynonymConfig {
            dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        });
        assert!(e.contains("x.toml"), "{e}");
    }
}
