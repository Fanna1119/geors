# Synonym and abbreviation rules

Query-time expansions, so that `hauptstr. 5` finds *Hauptstraße 5* and
`long st` finds *Long Street*. One file per language (ISO 639-1 code), all
compiled into the binary; no Rust changes are needed to add or edit them.

```toml
# xx.toml

[words]
# A whole word typed by the user -> what it may stand for.
str = ["strasse"]
st = ["street", "saint"]

[suffixes]
# The end of a longer word -> replacement: "hauptstr" -> "hauptstrasse".
str = ["strasse"]
```

Rules:

- Keys and values are normalised like the search index: lowercase, accents
  folded (`straße` = `strasse`, `plaça` = `placa`), punctuation removed. Write
  them either way.
- Each key and value must be a **single word**. Multi-word expansions are not
  supported, and the test suite rejects them.
- Expansions are alternatives: the typed word still matches as well.
- All languages apply to all data by default. Ambiguous abbreviations are
  fine (`dr` -> drive / doctor / doktor), because the extra alternatives only
  cost a little search time. Use `[synonyms] languages = [...]` in the server
  config to limit them.
- A suffix rule needs at least two characters before the suffix.

Adding a language: create `xx.toml`, then run `cargo test -p geors-index`
(it validates every file). Check the result with
`geors synonyms <words...>`.

Rules can also be added without rebuilding, in the server config: put more
files in a directory (`[synonyms] dir = "..."`), or write them inline
(`[synonyms.words]`, `[synonyms.suffixes]`).
