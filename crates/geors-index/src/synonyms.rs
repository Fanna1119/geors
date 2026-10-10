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
    /// Whole words: typed word -> what it may stand for.
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
    /// Inline rules (`[synonyms.words]`, `[synonyms.suffixes]`).
    pub words: BTreeMap<String, Vec<String>>,
    pub suffixes: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct Synonyms {
    words: HashMap<String, Vec<String>>,
    /// (suffix, replacements), longest suffix first.
    suffixes: Vec<(String, Vec<String>)>,
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
                s.add_file(&mut analyzer, &format!("built-in {lang}.toml"), text)?;
            }
        }
        if let Some(dir) = &config.dir {
            for path in rule_files(dir)? {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                s.add_file(&mut analyzer, &path.display().to_string(), &text)?;
            }
        }
        let inline = RuleSet {
            words: config.words.clone(),
            suffixes: config.suffixes.clone(),
        };
        s.add_rules(&mut analyzer, "[synonyms] in the config", &inline)?;
        s.suffixes.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
        Ok(s)
    }

    fn add_file(
        &mut self,
        analyzer: &mut TextAnalyzer,
        name: &str,
        text: &str,
    ) -> Result<(), String> {
        let rules: RuleSet = toml::from_str(text).map_err(|e| format!("{name}: {e}"))?;
        self.add_rules(analyzer, name, &rules)
    }

    fn add_rules(
        &mut self,
        analyzer: &mut TextAnalyzer,
        name: &str,
        rules: &RuleSet,
    ) -> Result<(), String> {
        let mut normalized = |key: &str,
                              values: &[String]|
         -> Result<(String, Vec<String>), String> {
            let key = normalize(analyzer, key).map_err(|e| format!("{name}: key {e}"))?;
            let values = values
                .iter()
                .map(|v| {
                    normalize(analyzer, v).map_err(|e| format!("{name}: value {e} (for '{key}')"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((key, values))
        };
        for (key, values) in &rules.words {
            let (key, values) = normalized(key, values)?;
            merge_into(self.words.entry(key).or_default(), values);
        }
        for (key, values) in &rules.suffixes {
            let (key, values) = normalized(key, values)?;
            let slot = match self.suffixes.iter().position(|(k, _)| *k == key) {
                Some(i) => &mut self.suffixes[i].1,
                None => {
                    self.suffixes.push((key, Vec::new()));
                    &mut self.suffixes.last_mut().unwrap().1
                }
            };
            merge_into(slot, values);
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty() && self.suffixes.is_empty()
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

fn merge_into(slot: &mut Vec<String>, values: Vec<String>) {
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

    /// Every shipped rule file must parse and contain only single words.
    #[test]
    fn builtin_files_are_valid() {
        assert!(builtin_languages().len() >= 5);
        for (lang, text) in builtin::BUILTIN {
            let mut s = Synonyms::default();
            s.add_file(&mut text::analyzer(), lang, text)
                .unwrap_or_else(|e| panic!("synonyms/{lang}.toml: {e}"));
            assert!(!s.is_empty(), "synonyms/{lang}.toml has no rules");
        }
    }

    #[test]
    fn builtin_rules_across_languages() {
        let s = Synonyms::builtin();
        let hauptstr = s.expand("hauptstr");
        assert!(hauptstr.contains(&"hauptstrasse".to_string()));
        assert!(hauptstr.contains(&"hauptstraat".to_string()));
        assert!(s.expand("st").contains(&"street".to_string()));
        assert!(s.expand("st").contains(&"sankt".to_string()));
        assert_eq!(s.expand("cres"), vec!["crescent"]);
        assert!(s.expand("vaduz").is_empty());
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
        assert_eq!(s.expand("st"), vec!["sankt"]);
        assert_eq!(s.expand("kh"), vec!["krankenhaus"]);
        assert_eq!(s.expand("kirchgs"), vec!["kirchgasse"]);
        assert!(s.expand("gr").contains(&"gross".to_string()));
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
        cfg.words.insert("rue".into(), vec!["rue de la".into()]);
        assert!(bad(cfg).contains("not a single word"));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.toml"), "[wrods]\na = [\"b\"]\n").unwrap();
        let e = bad(SynonymConfig {
            dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        });
        assert!(e.contains("x.toml"), "{e}");
    }
}
