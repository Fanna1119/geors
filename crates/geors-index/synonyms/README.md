# Synonym and abbreviation rules

Query-time expansions, so that `hauptstr. 5` finds *Hauptstraße 5* and
`long st` finds *Long Street*. One file per language (ISO 639-1 code), all
compiled into the binary; no Rust changes are needed to add or edit them.

```toml
# xx.toml

# Partitions of these countries use the rules (ISO codes); omit for all.
countries = ["xx", "yy"]

[words]
# A whole word typed by the user -> what it may stand for.
str = ["strasse"]
st = ["street", "saint"]
# ... or a phrase: matches places with all these words in one field.
olv = ["onze lieve vrouw"]

[suffixes]
# The end of a longer word -> replacement: "hauptstr" -> "hauptstrasse".
str = ["strasse"]
```

Rules:

- Keys and values are normalised like the search index: lowercase, accents
  folded (`straße` = `strasse`, `plaça` = `placa`), punctuation removed. Write
  them either way.
- Keys and suffix values must be a **single word**; `[words]` values may be
  phrases. Multi-word keys ("rue de la" -> ...) are not supported.
- Expansions are alternatives: the typed word still matches as well.
- A file applies to the partitions of its `countries` (all if omitted: the
  English rules, rule files from `dir`, inline rules). Ambiguous
  abbreviations are fine (`dr` -> drive / doctor / doktor), because the extra
  alternatives only cost a little search time. In the server config,
  `[synonyms] languages = [...]` limits the built-in files and
  `everywhere = true` ignores `countries`.
- A suffix rule needs at least two characters before the suffix.

Adding a language: create `xx.toml`, then run `cargo test -p geors-index`
(it validates every file). Check the result with
`geors synonyms <words...>` (add `--country xx` to see one country's rules).

Rules can also be added without rebuilding, in the server config: put more
files in a directory (`[synonyms] dir = "..."`), or write them inline
(`[synonyms.words]`, `[synonyms.suffixes]`).
