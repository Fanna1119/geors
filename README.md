# geors

A lightweight, fast geocoder for OpenStreetMap data, written in Rust and inspired by
[Photon](https://github.com/komoot/photon). It is built for
country and region extracts (up to a continent: Europe imports in under half
an hour on a laptop), and treats spatial filtering and per-country
partitioning as core features. Imports also run in small containers:
South Africa imports in 70 s within 256 MB of RAM and one CPU.

- Full-text search with search-as-you-type, typo tolerance and abbreviation expansion
  (`hauptstr.` → `hauptstrasse`), using [tantivy](https://github.com/quickwit-oss/tantivy)
- Spatial filters: radius, bounding box, country, layer
- Reverse geocoding and exact k-nearest-neighbour search, with distances measured to street geometry
- Address hierarchy (district / city / county / state / country, postcode) built from OSM boundaries
- Lookup by OSM id, and optional full geometry output (lines and polygons)
- Localised names (`lang=de`, `lang=ru`, …)
- Imports OSM `.osm.pbf` files and Nominatim/Photon JSON dumps
- **Continuous updates**: sources are tracked, new upstream versions are detected and
  re-imported, and the running server hot-swaps the data with no downtime
- One country per partition, so you load only the countries you need
- Single ~14 MB binary (70 MB distroless Docker image). All index files are
  memory-mapped.

### Test hardware

All numbers below come from one laptop, not a server:

| | |
|---|---|
| Machine | MacBook Pro, Apple M1 Pro: 8 cores (6 performance + 2 efficiency), 16 GB RAM, internal SSD |
| OS | macOS 26 |
| Docker tests | Docker Desktop VM with 8 CPUs and 7.7 GB, limited per test with `--memory` / `--cpus` |

Imports trade time for memory. Node coordinates, scratch files and the
finished index are memory-mapped files or are streamed from disk, so the OS
can drop those pages whenever it needs the memory. Up to country size, heap
use is therefore low: South Africa imports within 256 MB, and a fast SSD
matters more than lots of RAM. Some structures still grow with the extract:
- the boundary polygons used to find each place's city / district / postcode
  (Europe: 472 k boundaries);
- the street-merging index (48 bytes per street segment; Europe: 26 M
  segments).

So a continent still needs several GB; Europe peaked at 10 GB on the 16 GB
test machine. Import times include writing the index.

> **Import memory is not serving memory.** The "Import memory" column is
> what *building* the index takes, once per import or update. Serving the
> finished index needs far less: all of Europe runs in about 230 MB at idle
> and 380 MB under load (see below). You can build the index on a larger
> machine and serve it from a small one. High import memory for continent
> extracts is a known limitation that I'm still working on (see
> [Known issues](#known-issues-and-roadmap)).

| Extract | PBF | Places | Import | Import memory¹ | Index on disk |
|---|---|---|---|---|---|
| Liechtenstein | 3 MB | 16 k | 0.5 s | ~70 MB | 4 MB |
| Austria | 774 MB | 3.1 M | 34 s | 0.7 GB | 0.8 GB |
| South Africa | 402 MB | 483 k | 11 s | 0.3 GB | 147 MB |
| Germany | 4.6 GB | 24.6 M | **4 min 40 s** | 4.0 GB | 5.6 GB |
| Europe (55 countries) | 33 GB | 149 M | **26 min 38 s** | 10.2 GB² | 26 GB |

¹ Peak memory footprint (heap) while importing, not while serving. Node
coordinates and scratch files are
memory-mapped and evictable. Peak extra disk during import is about the size
of the finished index.
² Measured with `bench/import-stats.sh ... --all-countries`, before the
current country rules (see "Partitions and border slivers"), which no longer
need that flag. The machine was
already using 3.2 GB of swap before the import, and it pushed that to 4.2 GB.
Peak disk during the import was 49 GB (index plus scratch files).

**Small containers** (South Africa, Docker, 1 CPU, default settings):

| Memory limit | Import | Of which writing the index |
|---|---|---|
| 256 MB | 70 s | 12 s |

The import adapts to the memory it finds (cgroup limit or RAM). When node
coordinates would not fit in memory, ways are joined with their nodes by
sorting instead of random lookups (`--node-lookup`). When scratch files are
larger than a quarter of memory, the index writer regroups them first, so it
reads them sequentially. Sort buffers and the text indexer's memory scale with
the limit.

**Serving Germany + Austria + Liechtenstein + Andorra** (27.7 M places)
with 32 concurrent connections (`bench/run.sh`):

| Query mix | req/s | p50 | p99 |
|---|---|---|---|
| full address ("Hauptstraße 5 Wien") | 2,163 | 12 ms | 58 ms |
| autocomplete prefix (1–6 letters) | 733 | 35 ms | 654 ms |
| with typos | 537 | 47 ms | 868 ms |
| name + location bias | 1,262 | 21 ms | 330 ms |
| category within 2 km radius | 815 | 34 ms | 144 ms |
| reverse | 34,733 | 0.8 ms | 8 ms |
| 10 nearest | 20,892 | 1.4 ms | 8 ms |

**Serving all of Europe** (55 partitions, 149 M places, 26 GB index) on the
same laptop, 32 concurrent connections, queries sampled from every 500th
place (`EVERY=500 bench/run.sh data-europe 20s 32`):

| Query mix | req/s | p50 | p99 |
|---|---|---|---|
| full address | 415 | 68 ms | 324 ms |
| autocomplete prefix | 136 | 218 ms | 523 ms |
| with typos | 88 | 348 ms | 636 ms |
| name + location bias | 442 | 69 ms | 166 ms |
| category within 2 km radius | 1,611 | 18 ms | 52 ms |
| reverse | 32,857 | 0.9 ms | 3.7 ms |
| 10 nearest | 19,068 | 1.6 ms | 4.7 ms |

- **Memory:** **232 MB** physical footprint at start and **380 MB** after the
  load test. For comparison, Photon's README recommends "at least 64GB RAM"
  for its planet-wide database (about 95 GB on disk). That is not a direct
  comparison: Europe is a large part of the planet, but not all of it.
- **Page cache:** RSS reached 7.3 GB. These are cached index pages, which the
  OS drops under pressure.
- **Errors:** none.
- **Spatial queries** (reverse, nearest, radius) are as fast as on a single
  country.
- **Text search** under load is about 5× slower than on Germany + Austria:
  a query without `country=` searches all 55 partitions.
- **Single client:** a lone search uses all cores, one partition per core.
  With one connection, p50 latency is 4.4 ms for a full address, 17 ms for
  autocomplete, 16 ms for typos and 4.2 ms with a location bias. Searching
  one partition after another took 19 / 48 / 74 / 13 ms. Under load, each
  search uses one core: splitting searches when all cores are busy only adds
  overhead (measured: 20 % fewer req/s at 32 connections).
- **Load generator:** wrk ran on the same 8 cores as the server.

Server memory (Germany + Austria + Liechtenstein + Andorra): **18 MB physical
footprint at start, ~150 MB under load**.
RSS grows to a few GB as the OS caches index pages; those pages are clean and
the OS drops them under memory pressure.

Search quality on the same data (`bench/accuracy.py`, `bench/ambiguity.py`):

| Check | Germany | Austria |
|---|---|---|
| exact address → right house at rank 1 | 100 % | 98.7 % |
| address with a typo | 98.3 % | 98.7 % |
| city name | 97.3 % | 94.0 % |
| POI name, searched near it | 95.3 % | 97.0 % |
| 60 % of a POI name, near it → top 5 | 79.7 % | 81.7 % |

2,916 settlement names exist in both Germany and Austria. Searching near one
of them returns that one first 99.8 % of the time, and `country=` is respected
100 % of the time.

## Quick start

```sh
# 1. Build
cargo build --release

# 2. Import: straight from Geofabrik, or from a local file
./target/release/geors import geofabrik:europe/liechtenstein geofabrik:europe/andorra
./target/release/geors info

# 3. Serve on http://127.0.0.1:2322, checking for new data every 6 hours
./target/release/geors serve --update-interval 6h
```

`geofabrik:<region>` expands to `https://download.geofabrik.de/<region>-latest.osm.pbf`.
Any URL or local path works too:

```sh
geors import https://download.geofabrik.de/europe/luxembourg-latest.osm.pbf
geors import ./osm/my-city.osm.pbf --default-country DE
geors import ./nominatim-export.jsonl.gz
```

Other extract sources: <https://download.geofabrik.de> (countries, regions) and
<https://extract.bbbike.org> or <https://protomaps.com/extracts> (cities / custom areas).
Luxembourg (~40 MB) makes a good next step up from Liechtenstein and Andorra.

## Keeping data up to date

Photon gets continuous updates by running on a Nominatim PostgreSQL database
that applies OSM replication diffs. geors has no database: each partition is an
immutable, memory-mapped index, rebuilt from the source extract. It does use
diffs, though, to avoid downloading the whole extract again:

1. **Track.** Every `geors import` registers its source in `<data>/sources.json`
   (opt out with `--no-track`), with the import flags and the version imported.
   For extracts that publish diffs, such as Geofabrik's (the replication URL
   and sequence number are in the PBF header), the PBF is kept as a *base*:
   - downloaded extracts go to `<data>/.base/<name>.osm.pbf`
   - local files are used where they are
2. **Diff update.** `geors update` reads the replication `state.txt`. If the
   base is behind, it downloads only the missing daily `.osc.gz` diffs, merges
   them into the base, then rebuilds the partition:
   - **Size:** for Germany a diff is ~6 MB, versus a 4.6 GB full download.
   - **Speed:** merging streams the sorted base, decoding and compressing on
     all cores. Austria (97 M elements) takes 11 s.
   - **Your files:** the merged result always goes to `<data>/.base/`, so a
     local file you imported from is never modified.
3. **Fallback.** A full download happens (with `.md5` check) when a source has
   no diffs, is more than 90 diffs behind (Geofabrik keeps diffs 100 days),
   or a diff step fails.
   - **Plain URLs:** checked by `ETag` / `Last-Modified` / size.
   - **Local files without replication info:** checked by modification time.
4. **Hot reload.** Every rebuild bumps `<data>/GENERATION`. Running servers poll
   that file (every 30 s by default), open the new partitions and swap them in.
   Requests in flight finish on the old data. In testing, 2,875 requests during a
   swap all succeeded.

**Correctness of the diff merge:** a week-old official extract plus 7
published diffs was compared, element by element, with the current official
extract. Liechtenstein matched exactly on all 404,839 elements, and Austria on
all 97,480,621. The dev tool `cargo run --release -p geors-update --example
verify_diffs -- OLD.osm.pbf NEW.osm.pbf` repeats this check.

**Cost:** diffs save bandwidth, not rebuild time. Every update re-imports the
whole extract (Germany about 5 minutes, Europe about 30); see
[Known issues](#known-issues-and-roadmap) for why and for the plan. The base PBF also stays on disk. After the first diff update
of a local file, the managed copy in `<data>/.base/` takes over and your
original download is redundant. Let geors delete it with `prune_originals =
true` under `[updates]` in the config, or `geors update --prune-originals`.
Only files whose data is older than the managed copy are deleted, and the
source is then pointed at the copy. To always re-download in full and keep no
base, use `geors import --no-diffs`.

geors also cleans up its own leftovers in the data directory on every import
and update: scratch files of interrupted imports, half-written downloads, and
base files of sources removed with `geors sources --remove`.

You can run updates in either of two ways:

```sh
# a) Inside the server
geors serve --update-interval 6h          # or [updates] enabled = true in the config

# b) Externally, e.g. from cron or a systemd timer; the server picks the change up
geors update                               # all sources (diffs where possible)
geors update --sources liechtenstein       # one source
geors update --force                       # rebuild even if unchanged
geors update --full                        # download in full instead of diffs
geors update --prune-originals             # delete local extracts the managed copy replaced
```

```cron
# m h  dom mon dow  command
15 */6 *   *   *    cd /srv/geors && geors update --data /srv/geors/data >> update.log 2>&1
```

`geors sources` shows each source with its data timestamp, sequence number,
diff state and base file, last check/import time, and the last error if there
was one. `geors sources --remove NAME` stops tracking a source. A lock file
prevents concurrent imports or updates on the same data directory. `/status`
reports the loaded generation and the source states.

**Freshness compared with Photon/Nominatim:** geors data is as fresh as the
latest published diff: daily for Geofabrik extracts. Minutely updates would
require applying changes to a mutable index, which runs against geors's
low-memory, immutable-index design.

## Example requests

```sh
# Free text, search-as-you-type, typos, abbreviations
curl 'localhost:2322/search?q=vad'
curl 'localhost:2322/search?q=kunstmusem'
curl 'localhost:2322/search?q=städtle+35+vaduz'
curl 'localhost:2322/search?q=st+mamerten'              # st -> sankt

# Bias toward a location (no hard filter)
curl 'localhost:2322/search?q=bahnhof&lat=47.20&lon=9.55'

# Hard radius filter (metres) around a point
curl 'localhost:2322/search?q=restaurant&lat=47.141&lon=9.521&radius=300&limit=5'

# Bounding box: min_lon,min_lat,max_lon,max_lat
curl 'localhost:2322/search?q=hotel&bbox=1.50,42.50,1.54,42.52'

# Country and layer filters
curl 'localhost:2322/search?q=hotel&country=AD'
curl 'localhost:2322/search?q=landstrasse&layers=street'

# No text: everything in an area, nearest first
curl 'localhost:2322/search?lat=47.141&lon=9.521&radius=100&layers=poi'

# Full geometry (LineString / Polygon / Multi*) instead of the centre point
curl 'localhost:2322/search?q=liechtenstein&layers=country&geometry=full'

# Localised output
curl 'localhost:2322/search?q=vaduz&lang=ru'

# Reverse geocoding: what is here?
curl 'localhost:2322/reverse?lat=47.1394&lon=9.5226'
curl 'localhost:2322/reverse?lat=47.1394&lon=9.5226&radius=100&layers=street&limit=3'

# k nearest neighbours (exact, any distance)
curl 'localhost:2322/nearest?lat=42.507&lon=1.521&limit=5&layers=city'

# Lookup by OSM id (N=node, W=way, R=relation)
curl 'localhost:2322/lookup?osm_ids=N1932181216,W28712148'

# Loaded partitions, data generation, source/update state
curl 'localhost:2322/status'
```

## API

Every endpoint returns a GeoJSON `FeatureCollection`, except `/status`. Errors
return `{"error": "..."}` with status 400 for bad input or 500 for internal
errors. All endpoints accept `lang` and `geometry=point|full`.

### `GET /search` (alias `GET /api`)

| Parameter      | Description                                                                 |
|----------------|-----------------------------------------------------------------------------|
| `q`            | Free-text query. Optional if a location or bbox is given.                   |
| `lat`, `lon`   | Focus point. Ranks nearby results higher and is the centre for `radius`.    |
| `radius`       | Hard filter: metres from `lat`/`lon`.                                       |
| `bbox`         | Hard filter: `min_lon,min_lat,max_lon,max_lat`.                             |
| `country`      | ISO 3166-1 alpha-2 code(s), comma-separated (`DE`, `li,at`).               |
| `layers`       | Comma-separated: `house, poi, street, locality, district, city, county, state, country`. |
| `limit`        | Defaults to 10, maximum 50 (configurable).                                  |
| `lang`         | Language for names, such as `en` or `de`. Falls back to the default name.   |
| `autocomplete` | Defaults to `true`. Treats the last word as a prefix unless the query ends with a space. |
| `geometry`     | `point` (default) or `full`: lines for streets and rivers, polygons for areas, buildings and boundaries. With `full`, the centre point moves to the `center` property. |

Without `q`, the endpoint returns the places inside the spatial filter. With
`lat`/`lon` they come nearest first; with only a bbox they come most important
first.

### `GET /reverse`

`lat`, `lon` (required), `radius` (default 1000 m), `limit` (default 1),
`layers` (default `house,poi,street,locality`), `country`, `lang`, `geometry`.
Returns the closest places within the radius. For streets, the distance is
measured to the street's line geometry, not to its centre.

### `GET /nearest`

`lat`, `lon` (required), `limit` (default 5), `radius` (optional cap),
`layers`, `country`, `lang`, `geometry`. Returns the exact k nearest places at
any distance.

### `GET /lookup`

`osm_ids=N123,W456,R789` (required, up to `max_limit` ids), `country`, `lang`,
`geometry`. Results come back in request order; unknown ids are skipped. A
merged street can be found by the id of any of its OSM way segments.

### Feature properties

```json
{
  "type": "Feature",
  "geometry": { "type": "Point", "coordinates": [9.5221112, 47.1394963] },
  "properties": {
    "name": "Kunstmuseum Liechtenstein",
    "housenumber": "32", "street": "Städtle", "postcode": "9490",
    "city": "Vaduz", "county": "Oberland", "country": "Liechtenstein", "countrycode": "LI",
    "type": "poi",
    "osm_type": "W", "osm_id": 28712148, "osm_key": "tourism", "osm_value": "museum",
    "extent": [9.5217145, 47.1393789, 9.522507, 47.1396136],
    "distance": 169.2, "score": 20.7156, "importance": 0.3
  }
}
```

`extent` is `[min_lon, min_lat, max_lon, max_lat]`, following GeoJSON bbox
order (Photon uses a different order). `distance` is in metres and appears
only when a location was given.

## Configuration

Everything works with CLI flags. For the server you can also use a TOML file;
CLI flags override it. See [`geors.example.toml`](geors.example.toml) for
every option: API limits, ranking weights, update interval, reload polling,
and synonym rules.

### Abbreviations and synonyms

Rules such as `str` → `strasse`, `st` → `street` / `saint` / `sankt`, and
suffix rules (`kerkstr` → `kerkstraat`) are **data, not code**:

- **Built-in rules:** one TOML file per language in [`synonyms/`](synonyms/),
  currently af, ca, de, en, es, fr, it and nl. Every file there is embedded in
  the binary at build time, so adding a language means adding a file. The
  test suite validates each file; see [`synonyms/README.md`](synonyms/README.md)
  for the format.
- **Configuration:** the `[synonyms]` section of the server config can
  restrict the built-in languages (`languages = ["de", "en"]`), load extra
  rule files from a directory (`dir = ...`), and add inline rules
  (`[synonyms.words]`, `[synonyms.suffixes]`).
- **Checking rules:** `geors synonyms <words...> [-c geors.toml]` shows how
  words expand, for example `kerkstr  ->  kerkstraat, kerkstrasse`.

```sh
geors serve --config geors.toml
geors serve --data data --countries li,ad --bind 0.0.0.0:2322 --update-interval 1d
GEORS_DATA=/srv/geors geors serve               # env vars: GEORS_DATA, GEORS_BIND
RUST_LOG=debug geors serve                      # logging to stderr (default: info)
```

### Commands

| Command                    | Purpose                                                              |
|----------------------------|----------------------------------------------------------------------|
| `geors import INPUT...`    | Import files, URLs or `geofabrik:<region>` and register them as sources |
| `geors update`             | Re-import sources whose upstream changed (`--force`, `--sources a,b`) |
| `geors sources`            | List tracked sources with version and update state (`--remove NAME`) |
| `geors serve`              | HTTP server (`--update-interval`, `--config`, `--countries`)         |
| `geors info`               | Partitions in the data directory, with sizes and layer counts        |
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

#### Partitions and border slivers

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

### Nominatim / Photon dumps

geors reads the [Nominatim dump format](https://github.com/komoot/photon/blob/master/docs/json-dump-format-0.1.0.md)
that Photon imports from (`.json`, `.jsonl`, or gzip-compressed `.jsonl.gz`).
This lets you reuse Nominatim's address hierarchy instead of geors's own
boundary processing. It supports `CountryInfo`, `address_type` and
`rank_address`, localised `address` keys (`city:de`), indirect
`addresslines`, `bbox` and GeoJSON `geometry`. Places are partitioned by
`country_code`, like PBF imports.

## Architecture

```
crates/
  geors-core     types and models: Layer, Place, AdminUnit, geometry helpers, on-disk format
  geors-ingest   PBF and Nominatim dump -> places: classification, multipolygons, hierarchy, postcodes, street merging
  geors-index    partition writer/reader, packed R-tree, tantivy text index, synonyms, query engine
  geors-rank     scoring model (text x importance x proximity x name match)
  geors-update   import pipeline, source registry, change detection, downloads,
                 replication diffs (OSC parser, PBF writer, streaming merge), data-dir lock
  geors-api      axum HTTP server, hot-reloadable state, validation, GeoJSON rendering
src/main.rs      CLI: import / update / sources / serve / info / export
bench/           load, accuracy and ambiguity benchmarks (wrk + Python)
```

### Ingestion (`geors-ingest`)

1. **Five parallel streaming passes** over the PBF with `osmpbf`, so memory
   does not grow with the number of places:
   1. relations: admin and postcode boundaries, named multipolygons. This
      pass also records which blobs hold nodes and which hold ways, so later
      passes only decompress the blobs they need.
   2. ways: only the *ids* of needed nodes are recorded, plus node lists of
      relation member ways and `place=*` areas. The ids are deduplicated by
      an external merge sort: sorted runs on disk, merged sequentially.
   3. nodes: coordinates of the needed nodes go into a disk-backed, sorted
      store (16 bytes/node, memory-mapped), plus `place=*` nodes
   4. way features, then 5. node features: each place is built and gets its
      hierarchy on a worker thread, then is **spilled to a per-country
      scratch file**

   Between passes 3 and 4, boundary polygons and their point-in-polygon
   indexes are built on all cores.

   **Node lookup modes.** In `memory` mode, pass 4 looks up each way's nodes
   in the store above. That is a random read per node, which is fast while the
   store fits in RAM and very slow once it does not. In `sorted` mode, pass 2
   instead records `(node, way, position)` for every node reference and sorts
   it by node id. Pass 3 walks the nodes in file order alongside it, producing
   `(way, position, coordinate)`, which is sorted by way into a geometry file
   that pass 4 reads front to back. This costs extra temporary disk and
   sorting time, so `auto` (the default) only picks it when the store would
   exceed a quarter of the available memory. Both modes produce identical
   output. `sorted` needs nodes sorted by id, as in Geofabrik and planet
   files.

   Blobs are decoded on all cores (rayon). One consumer thread does the
   ordered writes through a bounded channel. Output is deterministic:
   repeated imports produce byte-identical partitions. The writer orders
   places by `(Z-order, OSM type, OSM id)` and keeps only that sort key in
   memory, never the arrival order. If a scratch file is larger than a
   quarter of memory, the writer first copies it, in one sequential pass,
   into buckets of consecutive Z-order ranges, each small enough to read
   into RAM whole. Places are then never read from disk one at a time.
   (Importance scores can differ in the last float digit between macOS and
   Linux, because `log10` differs slightly between their maths libraries.)
2. **Classification** turns OSM tags into layers: `place=*`, admin boundaries,
   named highways (`street`), named amenities/shops/tourism/… (`poi`), and
   `addr:housenumber` (`house`). Each place also gets an importance prior from
   its layer, place type, population and wikidata.
3. **Geometry**: streets keep simplified line geometry, used for exact reverse
   distances and for `geometry=full`. Areas keep simplified polygons with holes;
   the tolerance scales with the area's size (~1 m for buildings, tens of
   metres for countries). Relations are assembled into multipolygons.
4. **Hierarchy**: boundary polygons are found by point-in-polygon. Each
   boundary's edges are bucketed into latitude bands, so a test only checks
   the edges in the point's band. Robust orientation predicates make it
   exact, and points on a border count as inside. Results are cached per grid
   cell (bounded, two generations), so only cells that a boundary crosses need
   an exact test. The country comes from the country boundary, or, where
   the extract cuts that boundary off, from the `ISO3166-2` code of a
   complete region (`RU-MOS` → `ru`). Where a city or district has no
   boundary, the nearest `place=*` node
   within a plausible radius is used instead. Places without `addr:postcode`
   get one from a `boundary=postal_code` area when the extract has them. A
   boundary and its label node are merged into one result, which takes the
   boundary's polygon.
5. **Street and waterway merging**: segments with the same name in the same
   city (rivers and streams: along their whole length), lying within ~110 m
   of each other, become one place. Segments are spilled with a
   48-byte in-memory index, then merged one name group at a time. Clustering
   uses a spatial grid, so it is roughly linear instead of O(n²). Every
   segment's way id stays findable through `/lookup`.
6. **Partitioning** by country code, with a separate admin table for each
   partition.

### Partition layout (`data/<cc>/`)

| File          | Content                                                                  |
|---------------|--------------------------------------------------------------------------|
| `meta.json`   | Format version, counts, bbox, source                                     |
| `places.bin`  | Fixed 40-byte record per place: position, layer, importance, geometry kind, offsets |
| `docs.bin`    | Compact binary documents (postcard): names, address, OSM ids. Fields already in `places.bin` are omitted |
| `geom.bin`    | Line geometry (with a bbox per part) and polygon geometry, i32 fixed point |
| `tags.json`   | Interned `(osm_key, osm_value)` pairs referenced by documents            |
| `spatial.idx` | Packed Hilbert R-tree over place bboxes ([geo-index](https://github.com/kylebarron/geo-index), flatbush ABI) |
| `admin.json`  | Admin units with localised names, referenced by places                   |
| `text/`       | tantivy index, one segment: names, address context, house numbers, edge n-grams for autocomplete, OSM ids, a `place_id` fast field; no stored documents |

`data/` also holds `GENERATION` (the reload marker), `sources.json` (the source
registry) and `.lock`. Everything except the small JSON files is
memory-mapped, so the OS page cache holds the data instead of the heap.

### Query path (`geors-index::engine`)

1. **Partition pruning.** `country=` picks partitions. Partitions whose extent
   misses the spatial filter are skipped.
2. **Hard spatial filter.** The R-tree returns the radius/bbox candidates. Each
   candidate is checked exactly (haversine distance, or distance to the line
   for streets) and goes into a bitset.
3. **Text search on the candidate set.** Each query word must match. Work is
   staged so typical queries stay cheap:
   - **Phase 1, exact:** exact terms, abbreviation expansions, and for the
     last word a prefix. Prefixes come from an edge n-gram field (one term
     lookup). Prefixes of 1–2 letters only match important places, so
     typing "m" suggests München rather than scanning millions of names. A
     lone word is matched against names only. Address context (city,
     country) can prefix-match only when other words narrow the search.
   - **Phase 2, fuzzy:** runs only if phase 1 found nothing. Edit distance
     1–2 for words of 4+ letters, never for numbers, and only for words the
     index does not contain as typed.
   - **Phase 3:** one word may be missing, at a score penalty.
   - The fields are name, alternative/translated names, address context and
     house number. Per word, only the best-matching field counts (dis-max).
   - tantivy filters on the bitset and scores
     `text × (1 + importance) × proximity` while collecting.
4. **Re-rank** the merged top hits with an exact/prefix name-match boost, then
   remove near-duplicates.

`/nearest` and `/reverse` are exact best-first k-NN searches. They walk the
R-tree in order of a lower bound on distance, compute exact distances
(geometry-aware; line parts are skipped by their bbox), and stop once the
bound exceeds the k-th best. Every partition first contributes a few
candidates, so overlapping country extents near borders cannot force a long
walk. The integration tests check results against brute force.

## Development

```sh
cargo test --workspace     # unit and end-to-end tests (no downloads needed)
cargo clippy --workspace --all-targets
RUST_LOG=geors=debug cargo run --release -- serve
```

### Benchmarks

`bench/` generates queries from the data itself, so they work for any extract:

```sh
# Load: throughput and latency per query mix (needs wrk), plus server memory
bench/run.sh data 20s 32

# Quality: is the intended place ranked first?
geors export --data data --every 50 de | bench/accuracy.py http://127.0.0.1:2322 300

# Same-named places in two countries (city exports of both partitions)
bench/ambiguity.py http://127.0.0.1:2322 de.jsonl at.jsonl 200
```

### Static binary

The only native code is `ring` (TLS for downloads), which builds its own C and
assembly and supports musl. A fully static Linux binary therefore works:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

### Docker

```sh
docker build -t geors .
docker volume create geors-data
docker run --rm -v geors-data:/data -v ~/Downloads:/in:ro geors import /in/south-africa-latest.osm.pbf
docker run -d -p 2322:2322 -v geors-data:/data --memory 512m --cpus 1 -e GEORS_THREADS=1 geors
```

The image is distroless (binary only). Data lives in the `/data` volume.
`--memory` limits are detected and respected by the import. On macOS, use a
Docker volume rather than a bind mount for `/data`: bind mounts go through a
slow file-sharing layer, which makes every page-cache miss expensive.
`bench/docker-bench.sh` measures serving and importing under several memory
and CPU limits.

## Known issues and roadmap

Known limitations and planned work are tracked as
[GitHub issues](https://github.com/Fanna1119/geors/issues). The main ones:
- every update is a full re-import;
- importing a continent takes several GB of RAM (serving it does not).

Supported platforms: Linux and macOS.
