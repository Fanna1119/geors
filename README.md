# geors

A lightweight, fast geocoder for OpenStreetMap data, written in Rust and inspired by
[Photon](https://github.com/komoot/photon). It is built for **small extracts**
(a country, a region, a city), not the 100 GB planet file, and treats spatial
filtering and per-country partitioning as core features.

- Full-text search with search-as-you-type and typo tolerance ([tantivy](https://github.com/quickwit-oss/tantivy))
- Spatial filters: radius, bounding box, country, layer
- Reverse geocoding and exact k-nearest-neighbour search, distances measured to street geometry
- Address hierarchy (district / city / county / state / country) built from OSM boundaries
- Localised names (`lang=de`, `lang=ru`, …)
- One country per partition; load only the countries you need
- Single ~10 MB binary in pure Rust, with no C dependencies. All index files are memory-mapped.

Measured on a laptop with Liechtenstein + Andorra loaded: import takes **0.4 s per
country** with **~70 MB** peak memory. The server runs at **~15 MB RSS**, and a
search takes about **1 ms**.

## Quick start

```sh
# 1. Download a small extract (≈3.5 MB each) from Geofabrik
mkdir -p osm
curl -L -o osm/liechtenstein-latest.osm.pbf https://download.geofabrik.de/europe/liechtenstein-latest.osm.pbf
curl -L -o osm/andorra-latest.osm.pbf       https://download.geofabrik.de/europe/andorra-latest.osm.pbf

# 2. Build
cargo build --release

# 3. Build the indexes (one partition per country, written to ./data)
./target/release/geors import osm/liechtenstein-latest.osm.pbf osm/andorra-latest.osm.pbf --data data
./target/release/geors info --data data

# 4. Run the server (http://127.0.0.1:2322)
./target/release/geors serve --data data
```

Other extracts: <https://download.geofabrik.de> (countries, regions) and
<https://extract.bbbike.org> or <https://protomaps.com/extracts> (cities / custom areas).
Luxembourg (~40 MB) makes a good next step up from Liechtenstein and Andorra.

## Example requests

```sh
# Free text, search-as-you-type, typos
curl 'localhost:2322/search?q=vad'
curl 'localhost:2322/search?q=kunstmusem'
curl 'localhost:2322/search?q=städtle+35+vaduz'

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

# Localised output
curl 'localhost:2322/search?q=vaduz&lang=ru'

# Reverse geocoding: what is here?
curl 'localhost:2322/reverse?lat=47.1394&lon=9.5226'
curl 'localhost:2322/reverse?lat=47.1394&lon=9.5226&radius=100&layers=street&limit=3'

# k nearest neighbours (exact, any distance)
curl 'localhost:2322/nearest?lat=42.507&lon=1.521&limit=5&layers=city'

# Loaded partitions
curl 'localhost:2322/status'
```

## API

Every endpoint returns a GeoJSON `FeatureCollection`, except `/status`. Errors
return `{"error": "..."}` with status 400 for bad input or 500 for internal
errors.

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

Without `q`, the endpoint returns the places inside the spatial filter. With
`lat`/`lon` they come nearest first; with only a bbox they come most important
first.

### `GET /reverse`

`lat`, `lon` (required), `radius` (default 1000 m), `limit` (default 1),
`layers` (default `house,poi,street,locality`), `country`, `lang`. Returns the
closest places within the radius. For streets, the distance is measured to the
street's line geometry, not to its centre.

### `GET /nearest`

`lat`, `lon` (required), `limit` (default 5), `radius` (optional cap),
`layers`, `country`, `lang`. Returns the exact k nearest places at any
distance.

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
CLI flags override it. See [`geors.example.toml`](geors.example.toml).

```sh
geors serve --config geors.toml                 # file
geors serve --data data --countries li,ad --bind 0.0.0.0:2322
GEORS_DATA=/srv/geors geors serve               # env vars: GEORS_DATA, GEORS_BIND
RUST_LOG=debug geors serve                      # logging (default: info)
```

`geors import` flags:

| Flag                   | Meaning                                                                         |
|------------------------|---------------------------------------------------------------------------------|
| `--data DIR`           | Output directory (default `data`).                                             |
| `--countries li,ch`    | Keep only these countries from the extract.                                    |
| `--all-countries`      | Keep every country found, including slivers across the border.                |
| `--default-country XX` | Assign places outside every country boundary to `XX` (useful for city extracts without the country boundary). |

By default, an import keeps every country that holds at least 1% of the
extract's places. This drops the few neighbouring-country places that a border
buffer pulls in. Without that rule, importing `liechtenstein.osm.pbf` would
create a 24-place `at` partition that overwrites a real Austria import.
Re-importing a country replaces its partition atomically.

## Architecture

```
crates/
  geors-core     types and models: Layer, Place, AdminUnit, geometry helpers, on-disk format
  geors-ingest   PBF -> places: tag classification, multipolygons, hierarchy, street merging
  geors-index    partition writer/reader, packed R-tree, tantivy text index, query engine
  geors-rank     scoring model (text x importance x proximity x name match)
  geors-api      axum HTTP server, parameter validation, GeoJSON rendering
src/main.rs      CLI: import / serve / info
```

### Ingestion (`geors-ingest`)

1. **Three streaming passes** over the PBF with `osmpbf`:
   - relations: admin boundaries and named multipolygons
   - ways: features, plus member ways of those relations
   - nodes: node features, plus coordinates of the nodes needed by those ways

   Only the needed node coordinates are kept in memory, as a sorted id array
   with binary search.
2. **Classification** turns OSM tags into layers: `place=*`, admin boundaries,
   named highways (`street`), named amenities/shops/tourism/… (`poi`), and
   `addr:housenumber` (`house`). Each place also gets an importance prior from
   its layer, place type, population and wikidata.
3. **Geometry**: streets keep simplified line geometry for exact reverse
   distances. Areas get an interior point and an extent. Relations are
   assembled into multipolygons, with holes.
4. **Hierarchy**: boundary polygons are found by point-in-polygon. Results are
   cached per grid cell, so only cells that a boundary crosses need an exact
   test. Where a city or district has no boundary, the nearest `place=*` node
   within a plausible radius is used instead. A boundary and its label node
   are merged into one result.
5. **Street merging**: segments with the same name in the same city, lying
   within ~110 m of each other, become one street.
6. **Partitioning** by country code, with a separate admin table for each
   partition.

### Partition layout (`data/<cc>/`)

| File          | Content                                                                  |
|---------------|--------------------------------------------------------------------------|
| `meta.json`   | Format version, counts, bbox, source                                     |
| `places.bin`  | Fixed 40-byte record per place: position, layer, importance, offsets     |
| `docs.bin`    | JSON documents (name, address, OSM ids, …)                               |
| `geom.bin`    | Street/river line geometry (i32 fixed-point)                             |
| `spatial.idx` | Packed Hilbert R-tree over place bboxes ([geo-index](https://github.com/kylebarron/geo-index), flatbush ABI) |
| `admin.json`  | Admin units with localised names, referenced by places                   |
| `text/`       | tantivy index: postings plus a `place_id` fast field, no stored documents |

Everything except the two small JSON files is memory-mapped, so the OS page
cache holds the data instead of the heap.

### Query path (`geors-index::engine`)

1. **Partition pruning.** `country=` picks partitions. Partitions whose extent
   misses the spatial filter are skipped.
2. **Hard spatial filter.** The R-tree returns the radius/bbox candidates. Each
   candidate is checked exactly (haversine distance, or distance to the line
   for streets) and goes into a bitset.
3. **Text search on the candidate set.** Each query word must match:
   - Matching uses an exact term, a prefix (last word), or a fuzzy match
     (edit distance 1–2 for words with 4+ letters, never for numbers).
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

The dependency tree is pure Rust, so on Linux you can build a fully static
binary:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Limitations and roadmap

- Ranking is hand-tuned and simple. There are no abbreviation or synonym
  expansions yet (`str.` ↔ `strasse`). Transliteration is limited to accent
  folding; Cyrillic input matches through `name:*` translations.
- House numbers are matched as tokens. There is no interpolation (`addr:interpolation`).
- Address hierarchy needs boundary relations in the extract. For city extracts
  without them, use `--default-country`; city and district then come from
  `place=*` nodes.
- Ingestion keeps the places of one extract in memory. That is fine for any
  country extract, but not designed for the planet.
- Planned: Photon JSONL dump import (it maps directly onto `Place`), postcode
  boundaries, polygon output (`extent` → geometry), `/lookup` by OSM id,
  hot reload of partitions.
