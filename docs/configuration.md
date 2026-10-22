# Configuration and commands

[README](../README.md) · [API](api.md) · [Configuration](configuration.md) · [Updates](updates.md) · [Benchmarks](benchmarks.md) · [Architecture](architecture.md) · [Development](development.md)

Everything works with CLI flags. For the server you can also use a TOML file;
CLI flags override it. See [`geors.example.toml`](../geors.example.toml) for
every option: API limits, ranking weights, update interval, reload polling,
and synonym rules.

## Abbreviations and synonyms

Rules such as `str` → `strasse`, `st` → `street` / `saint` / `sankt`, and
suffix rules (`kerkstr` → `kerkstraat`) are **data, not code**:

- **Built-in rules:** one TOML file per language in [`crates/geors-index/synonyms/`](../crates/geors-index/synonyms/),
  currently af, ca, de, en, es, fr, it and nl. Every file there is embedded in
  the binary at build time, so adding a language means adding a file. The
  test suite validates each file; see [`synonyms/README.md`](../crates/geors-index/synonyms/README.md)
  for the format.
- **Per country:** each file lists the countries whose partitions use it
  (German: de, at, ch, li, lu, be, it; English: everywhere), so in South
  Africa `hauptstr` expands to *hauptstraat* (Afrikaans) and `st` to
  *street* / *saint*, without the German *strasse* and *sankt*.
  `everywhere = true` in the config applies every language everywhere.
- **Phrases:** a word may stand for several (`olv` → *onze lieve vrouw*,
  `nyc` → *new york city*); a place matches when all of them occur in one
  field.
- **Configuration:** the `[synonyms]` section of the server config can
  restrict the built-in languages (`languages = ["de", "en"]`), load extra
  rule files from a directory (`dir = ...`), and add inline rules
  (`[synonyms.words]`, `[synonyms.suffixes]`).
- **Checking rules:** `geors synonyms <words...> [-c geors.toml] [--country
  de]` shows how words expand, for example `kerkstr  ->  kerkstraat,
  kerkstrasse`.

```sh
geors serve --config geors.toml
geors serve --data data --countries li,ad --bind 0.0.0.0:2322 --update-interval 1d
GEORS_DATA=/srv/geors geors serve               # env vars: GEORS_DATA, GEORS_BIND
RUST_LOG=debug geors serve                      # logging to stderr (default: info)
```

## Commands

| Command                    | Purpose                                                              |
|----------------------------|----------------------------------------------------------------------|
| `geors import INPUT...`    | Import files, URLs or `geofabrik:<region>` and register them as sources |
| `geors update`             | Re-import sources whose upstream changed (`--force`, `--sources a,b`) |
| `geors sources`            | List tracked sources with version and update state (`--remove NAME`) |
| `geors serve`              | HTTP server (`--update-interval`, `--config`, `--countries`)         |
| `geors info`               | Partitions in the data directory, with sizes and layer counts        |
| `geors upgrade`            | Add files newer versions write at import (`importance.bin`, `centers.bin`) to existing partitions, without re-importing |
| `geors export CC`          | A partition's places as JSON lines (`--every N` samples), for inspection and diffs |
| `geors synonyms WORDS...`  | Show abbreviation / synonym expansions (`-c` to include config rules) |

`geors import` flags:

| Flag                   | Meaning                                                                         |
|------------------------|---------------------------------------------------------------------------------|
| `--data DIR`           | Data directory (default `data`).                                               |
| `--countries li,ch`    | Keep only these countries from the extract.                                    |
| `--all-countries`      | Keep every country found, including slivers across the border (see "Partitions and border slivers"). |
| `--default-country XX` | Assign places outside every country boundary to `XX` (useful for city extracts without the country boundary). |
| `--name NAME`          | Source name for `update` (default: derived from the file name).                |
| `--no-track`           | Import once, without registering the source for updates.                        |
| `--keep-download`      | Keep downloaded files in `<data>/.downloads`.                                   |
| `--no-diffs`           | Keep no base PBF; `update` always downloads the extract in full.                |
| `--importance FILE`    | Rank with Wikipedia importance (see below). Also `geors update --importance FILE`. |

These flags are stored with the source, so `geors update` re-imports it the
same way.

Global flags (any command, before or after it):

| Flag                     | Meaning                                                                       |
|--------------------------|-------------------------------------------------------------------------------|
| `--threads N`            | Worker threads (env `GEORS_THREADS`; default: all cores).                     |
| `--index-memory-mb N`    | Text-index writer memory per partition.                                       |
| `--node-lookup MODE`     | How way geometry finds node coordinates (env `GEORS_NODE_LOOKUP`): `memory` (memory-mapped coordinate store, random access), `sorted` (sort node references and join them with the nodes in one sequential sweep; costs extra temporary disk and sorting time but never reads the store at random), or `auto` (default: `sorted` when the coordinate store would exceed a quarter of RAM or the container limit). |

`sorted` needs a PBF whose nodes are sorted by id, which is true of
Geofabrik and planet extracts. Both modes produce identical output.

### Partitions and border slivers

Extracts include a buffer across their border, which pulls in a few places of
each neighbouring country. Writing those as partitions would replace a real
import of that country with a fragment: importing Liechtenstein would
overwrite an Austria import with 24 places. So, unless `--countries` or
`--all-countries` is given, an import keeps a country when:

1. **its complete boundary lies inside the extract**, and none of its regions
   are cut off. This is meant to keep every country of a continent extract,
   however small (Vatican City has 355 of Europe's 149 M places).
2. **complete regions of it lie inside the extract** (states, provinces,
   municipalities with an `ISO3166-2` code), and it has at least 0.1 % of the
   places. This keeps countries that the extract cuts through: Germany in a
   Bremen extract, and should keep European Russia and Turkey in the Europe
   extract (not yet verified).
3. **it has at least 1 % of the places.**

"Inside the extract" means inside the bounding box in the PBF header.
Completeness alone is not enough: Geofabrik's South Africa extract has a hole
for Lesotho, yet Lesotho's boundary is complete in it, because the border
lies in the buffer. Lesotho's districts are cut off, though, so it is
recognised as a fragment. Tested on Liechtenstein, Andorra, Bremen, South
Africa and Austria: each keeps exactly its own country.

Places in a country whose boundary *and* regions are all cut off by the
extract get no country and are skipped. This also applies to city extracts
without any complete boundary that has a country or `ISO3166-2` code. Use
`--default-country XX` to assign those places a country; their city and
district then come from `place=*` nodes.

Re-importing a country replaces its partition atomically.

#### Countries split between extracts

Some countries span two extracts: Russia is in both the Europe and the Asia
extract, and so are Turkey and Kazakhstan. The source that imports a
country first owns its partition, `<cc>`. When another source holds a
substantial part of the same country, that part is written next to it as
`<cc>@<source>` (for example `ru@asia`). Searches treat all parts as one
country (`country=ru` searches both), and places in the overlap between the
extracts are returned once. Re-importing either source replaces only its
own part.

A strip of a country that another source imports in full (the Aleutians of
the United States in the Asia extract, more than ten times fewer places) is
skipped instead: it would only duplicate places. If the existing partition
is the strip, the larger import takes it over. `--countries` always
replaces the `<cc>` partition. `geors info` and `/status` list each part;
`geors export ru@asia` exports one.

## Wikipedia importance

Ranking leans on each place's importance (0–1). Without extra data it comes
from the place type, population and whether the object links to Wikidata at
all. With Nominatim's Wikipedia importance file, a place linked to Wikidata
is ranked by how prominent its Wikipedia article is (computed from the links
pointing to it): Springfield, Illinois before Springfield, Massachusetts,
Portland, Oregon before Portland, Maine, and well-known landmarks before
same-named shops.

```sh
curl -LO -A "geors" https://nominatim.org/data/wikimedia-importance.csv.gz   # ~320 MB
geors import geofabrik:europe/austria --importance wikimedia-importance.csv.gz
# existing sources: set it and re-import them
geors update --force --importance wikimedia-importance.csv.gz
```

- The file is stored with each source, so `geors update` keeps using it
  (`GEORS_IMPORTANCE` sets it for `import` too).
- The first import parses it (about a minute) and caches the highest
  importance per Wikidata id in `<data>/.importance/` (about 30 MB); later
  imports load the cache in a fraction of a second.
- It only changes importances, so it needs a re-import to take effect. Use it
  for all sources of a data directory, or none, so countries stay comparable.
- A `wikidata` tag without a Wikipedia article gets no bonus with the file
  (about 0.1 without it); a `wikipedia` tag without `wikidata` keeps the flat
  bonus.

The importance data is derived from Wikipedia and published by the
[Nominatim](https://nominatim.org) project; Wikipedia content is available
under [CC BY-SA](https://creativecommons.org/licenses/by-sa/4.0/).

## Nominatim / Photon dumps

geors reads the [Nominatim dump format](https://github.com/komoot/photon/blob/master/docs/json-dump-format-0.1.0.md)
that Photon imports from (`.json`, `.jsonl`, or gzip-compressed `.jsonl.gz`).
This lets you reuse Nominatim's address hierarchy instead of geors's own
boundary processing. It supports `CountryInfo`, `address_type` and
`rank_address`, localised `address` keys (`city:de`), indirect
`addresslines`, `bbox` and GeoJSON `geometry`. Places are partitioned by
`country_code`, like PBF imports.
