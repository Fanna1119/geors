# geors

A lightweight, fast geocoder for OpenStreetMap data, written in Rust and inspired by
[Photon](https://github.com/komoot/photon). It is built for **small extracts**
(a country, a region, a city), not the 100 GB planet file, and treats spatial
filtering and per-country partitioning as core features.

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
- Single ~10 MB binary. All index files are memory-mapped.

Measured on a laptop with Liechtenstein + Andorra loaded: import takes **0.4 s per
country** with **~70 MB** peak memory. The server runs at **~13–15 MB RSS**, a
search takes about **1 ms**, and a hot reload takes about **2 ms**.

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
immutable, memory-mapped index, rebuilt from the source extract. For the extract
sizes geors targets, that takes well under a second per country, so the update
cycle is:

1. **Track.** Every `geors import` registers its source in `<data>/sources.json`
   (opt out with `--no-track`). The registry records the location, the import
   flags and the version that was imported.
2. **Detect.** `geors update` checks each source for a newer version:
   - Geofabrik URLs: the replication `state.txt` published next to the file
     (sequence number and data timestamp). Geofabrik publishes daily.
   - Other URLs: `ETag`, `Last-Modified` and `Content-Length`.
   - Local files: modification time and size. Overwrite the file and it gets
     picked up.
3. **Fetch and rebuild.** Changed sources are downloaded and checked against the
   published `.md5` (with one automatic retry if the file was replaced
   mid-download), then re-imported. Partitions are swapped atomically. A failing
   source is reported and the previous data stays in place.
4. **Hot reload.** Every rebuild bumps `<data>/GENERATION`. Running servers poll
   that file (every 30 s by default), open the new partitions and swap them in.
   Requests in flight finish on the old data. In testing, 2,875 requests during a
   swap all succeeded.

You can run updates in either of two ways:

```sh
# a) Inside the server
geors serve --update-interval 6h          # or [updates] enabled = true in the config

# b) Externally, e.g. from cron or a systemd timer; the server picks the change up
geors update                               # all sources
geors update --sources liechtenstein       # one source
geors update --force                       # rebuild even if unchanged
```

```cron
# m h  dom mon dow  command
15 */6 *   *   *    cd /srv/geors && geors update --data /srv/geors/data >> update.log 2>&1
```

`geors sources` shows each source with its data timestamp, sequence number and
last check/import time, plus the last error if there was one. `geors sources
--remove NAME` stops tracking a source. A lock file prevents concurrent imports
or updates on the same data directory. `/status` reports the loaded generation
and the source states.

**Freshness compared with Photon/Nominatim:** geors data is as fresh as the
published extract. For Geofabrik that is daily. Minutely updates would require
applying `.osc` diffs to a mutable store, which runs against geors's
low-memory, immutable-index design. For a country-sized extract, a daily full
rebuild is cheaper and simpler than keeping a database in sync.

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
and custom abbreviations.

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

`geors import` flags:

| Flag                   | Meaning                                                                         |
|------------------------|---------------------------------------------------------------------------------|
| `--data DIR`           | Data directory (default `data`).                                               |
| `--countries li,ch`    | Keep only these countries from the extract.                                    |
| `--all-countries`      | Keep every country found, including slivers across the border.                 |
| `--default-country XX` | Assign places outside every country boundary to `XX` (useful for city extracts without the country boundary). |
| `--name NAME`          | Source name for `update` (default: derived from the file name).                |
| `--no-track`           | Import once, without registering the source for updates.                        |
| `--keep-download`      | Keep downloaded files in `<data>/.downloads`.                                   |

These flags are stored with the source, so `geors update` re-imports it the
same way.

By default, an import keeps every country that holds at least 1% of the
extract's places. This drops the few neighbouring-country places that a border
buffer pulls in. Without that rule, importing Liechtenstein would create a
24-place `at` partition that overwrites a real Austria import. Re-importing a
country replaces its partition atomically.

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
  geors-update   import pipeline, source registry, change detection, downloads, data-dir lock
  geors-api      axum HTTP server, hot-reloadable state, validation, GeoJSON rendering
src/main.rs      CLI: import / update / sources / serve / info
```

### Ingestion (`geors-ingest`)

1. **Three streaming passes** over the PBF with `osmpbf`:
   - relations: admin and postcode boundaries, named multipolygons
   - ways: features, plus member ways of those relations
   - nodes: node features, plus coordinates of the nodes needed by those ways

   Only the needed node coordinates are kept in memory, as a sorted id array
   with binary search.
2. **Classification** turns OSM tags into layers: `place=*`, admin boundaries,
   named highways (`street`), named amenities/shops/tourism/… (`poi`), and
   `addr:housenumber` (`house`). Each place also gets an importance prior from
   its layer, place type, population and wikidata.
3. **Geometry**: streets keep simplified line geometry, used for exact reverse
   distances and for `geometry=full`. Areas keep simplified polygons with holes;
   the tolerance scales with the area's size (~1 m for buildings, tens of
   metres for countries). Relations are assembled into multipolygons.
4. **Hierarchy**: boundary polygons are found by point-in-polygon. Results are
   cached per grid cell, so only cells that a boundary crosses need an exact
   test. Where a city or district has no boundary, the nearest `place=*` node
   within a plausible radius is used instead. Places without `addr:postcode`
   get one from a `boundary=postal_code` area when the extract has them. A
   boundary and its label node are merged into one result, which takes the
   boundary's polygon.
5. **Street merging**: segments with the same name in the same city, lying
   within ~110 m of each other, become one street. Every segment's way id
   stays findable through `/lookup`.
6. **Partitioning** by country code, with a separate admin table for each
   partition.

### Partition layout (`data/<cc>/`)

| File          | Content                                                                  |
|---------------|--------------------------------------------------------------------------|
| `meta.json`   | Format version, counts, bbox, source                                     |
| `places.bin`  | Fixed 40-byte record per place: position, layer, importance, geometry kind, offsets |
| `docs.bin`    | JSON documents (name, address, OSM ids, …)                               |
| `geom.bin`    | Line and polygon geometry (i32 fixed point)                              |
| `spatial.idx` | Packed Hilbert R-tree over place bboxes ([geo-index](https://github.com/kylebarron/geo-index), flatbush ABI) |
| `admin.json`  | Admin units with localised names, referenced by places                   |
| `text/`       | tantivy index: postings, an OSM id field and a `place_id` fast field; no stored documents |

`data/` also holds `GENERATION` (the reload marker), `sources.json` (the source
registry) and `.lock`. Everything except the small JSON files is
memory-mapped, so the OS page cache holds the data instead of the heap.

### Query path (`geors-index::engine`)

1. **Partition pruning.** `country=` picks partitions. Partitions whose extent
   misses the spatial filter are skipped.
2. **Hard spatial filter.** The R-tree returns the radius/bbox candidates. Each
   candidate is checked exactly (haversine distance, or distance to the line
   for streets) and goes into a bitset.
3. **Text search on the candidate set.** Each query word must match:
   - Matching uses an exact term, an abbreviation expansion, a prefix (last
     word), or a fuzzy match (edit distance 1–2 for words with 4+ letters,
     never for numbers).
   - The fields are name, alternative/translated names, address context and
     house number. Per word, only the best-matching field counts (dis-max).
   - tantivy filters on the bitset and scores
     `text × (1 + importance) × proximity` while collecting.
   - If too few results come back, the search retries with one word allowed
     to be missing, at a penalty.
4. **Re-rank** the merged top hits with an exact/prefix name-match boost, then
   remove near-duplicates.

`/nearest` is exact. An approximate R-tree k-NN (longitude scaled by
cos(lat)) gives a distance bound. A second, exact pass covers that radius. The
integration tests check this against brute force.

## Development

```sh
cargo test --workspace     # unit and end-to-end tests (no downloads needed)
cargo clippy --workspace --all-targets
RUST_LOG=geors=debug cargo run --release -- serve
```

### Static binary

The only native code is `ring` (TLS for downloads), which builds its own C and
assembly and supports musl. A fully static Linux binary therefore works:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Limitations and roadmap

- Updates rebuild whole partitions from the latest published extract (daily
  for Geofabrik). There is no minutely diff replication (see "Keeping data up
  to date").
- House numbers are matched as tokens. There is no interpolation (`addr:interpolation`).
- Transliteration is limited to accent folding. Cyrillic and other scripts
  match through `name:*` translations.
- For city extracts without boundary relations, use `--default-country`; city
  and district then come from `place=*` nodes.
- Ingestion keeps the places of one extract in memory. That is fine for any
  country extract, but not designed for the planet.
- On Windows, replacing a partition while a server has it open can fail. Run
  updates inside the server or stop it first. Linux and macOS are unaffected.
- Ideas: point-in-polygon reverse geocoding ("which city contains this
  point"), `/lookup` for Nominatim place ids, category filters from dump
  `categories`, a Prometheus metrics endpoint.
