# Architecture

[README](../README.md) · [API](api.md) · [Configuration](configuration.md) · [Updates](updates.md) · [Benchmarks](benchmarks.md) · [Architecture](architecture.md) · [Development](development.md)

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

## Ingestion (`geors-ingest`)

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
   indexes are built on all cores, in chunks, into memory-mapped scratch
   files (the heap keeps about 80 bytes per polygon). Relation outlines and
   the street-merging index are kept on disk too.

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
   places by `(Z-order, OSM type, OSM id)`; those sort keys are sorted on
   disk (an external merge sort), never held per place in RAM. If a scratch
   file is larger than a quarter of memory, the writer first copies it, in
   one sequential pass, into buckets of consecutive Z-order ranges, each
   small enough to read into RAM whole. Places are then never read from
   disk one at a time. The spatial index (`spatial.idx`, a packed Hilbert
   R-tree read with geo-index) is written by a streaming builder: leaves
   sorted on disk and streamed into the file, only inner nodes in RAM.
   (Importance scores can differ in the last float digit between macOS and
   Linux, because `log10` differs slightly between their maths libraries.)
2. **Classification** turns OSM tags into layers: `place=*`, admin boundaries,
   named highways (`street`), named amenities/shops/tourism/… (`poi`), and
   `addr:housenumber` (`house`). Each place also gets an importance (0–1)
   from its layer, place type and population, plus a bonus for a Wikidata
   link: with the Wikipedia importance file (`--importance`), 0.35 × the
   linked article's importance, otherwise a flat 0.1. Above 0.9 it
   approaches 1 smoothly, so large places keep their order.
   **House-number interpolation:** an `addr:interpolation` line
   (`odd`/`even`/`all`) between numbered address nodes stands for the
   numbers in between; they become `house` places (`osm_key`
   `addr:interpolation`) spread evenly along the line, between each pair of
   numbered nodes on the same street. South Africa: 42 k houses from 3.6 k
   lines.
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

## Partition layout (`data/<cc>/`)

| File          | Content                                                                  |
|---------------|--------------------------------------------------------------------------|
| `meta.json`   | Format version, counts, bbox, source                                     |
| `places.bin`  | Fixed 40-byte record per place: position, layer, importance, geometry kind, offsets |
| `importance.bin` | Importance per place (f32), copied from `places.bin`: read for every text match, small enough to stay cached |
| `centers.bin` | Centre per place (2 × i32), copied from `places.bin`: read for every text match when the search has a location |
| `docs.bin`    | Compact binary documents (postcard): names, address, OSM ids. Fields already in `places.bin` are omitted |
| `geom.bin`    | Line geometry (with a bbox per part) and polygon geometry, i32 fixed point |
| `tags.json`   | Interned `(osm_key, osm_value)` pairs referenced by documents            |
| `spatial.idx` | Packed Hilbert R-tree over place bboxes ([geo-index](https://github.com/kylebarron/geo-index), flatbush ABI) |
| `admin.json`  | Admin units with localised names, referenced by places                   |
| `text/`       | tantivy index, one segment: names, English names of areas, address context, house numbers, edge n-grams for autocomplete, OSM ids, a `place_id` fast field; no stored documents |

`data/` also holds `GENERATION` (the reload marker), `sources.json` (the source
registry) and `.lock`. Everything except the small JSON files is
memory-mapped, so the OS page cache holds the data instead of the heap.

## Query path (`geors-index::engine`)

1. **Partition pruning.** `country=` picks partitions. Partitions whose extent
   misses the spatial filter are skipped.
2. **Hard spatial filter.** The R-tree returns the radius/bbox candidates. Each
   candidate is checked exactly (haversine distance, or distance to the line
   for streets) and goes into a bitset.
3. **Text search on the candidate set.** Each query word must match. Work is
   staged so typical queries stay cheap; a later phase runs only if the
   earlier ones found nothing:
   - **Major places first:** a single word of 3–5 letters being typed, with
     no location, filter or layer, is first looked up among major places
     (cities and up, well-known POIs). Everything is searched only if they
     give fewer hits than requested.
   - **Phase 1, exact:** exact terms, abbreviation expansions, and for the
     last word a prefix. Prefixes come from an edge n-gram field (one term
     lookup). Prefixes of 1–2 letters only match important places, so
     typing "m" suggests München rather than scanning millions of names. A
     lone word is matched against names only. Address context (city,
     country) can prefix-match only when other words narrow the search.
   - **Phase 2, one typo:** edit distance 1 for words of 4+ letters, never
     for numbers, and only for words the index does not contain as typed.
   - **Phase 3, two typos:** edit distance 2 for words of 8+ letters (a word
     being typed allows one). Skipped when no word is that long.
   - **Phase 4:** one word may be missing, at a score penalty.
   - **Partition skipping:** before each phase, partitions where a word
     cannot match (not in the term dictionary as typed, as a synonym, split,
     prefix or within the typo distance) are skipped, so a query naming a
     city searches only the countries that have it.
   - The fields are name, the English name of an area (city and up, when it
     differs: "Vienna" for Wien, scored like the name), alternative and
     translated names, address context and house number. Per word, only the
     best-matching field counts (dis-max). Term statistics (BM25) are summed
     over all partitions, so scores compare across countries.
   - tantivy filters on the bitset and scores
     `text × (1 + importance) × proximity` while collecting. Importance and
     centre come from `importance.bin` and `centers.bin`, small enough to
     stay in the page cache when the index does not.
4. **Re-rank** the merged top hits with an exact/prefix name-match boost, then
   remove near-duplicates.

`/nearest` and `/reverse` are exact best-first k-NN searches. They walk the
R-tree in order of a lower bound on distance, compute exact distances
(geometry-aware; line parts are skipped by their bbox), and stop once the
bound exceeds the k-th best. Every partition first contributes a few
candidates, so overlapping country extents near borders cannot force a long
walk. The integration tests check results against brute force.
