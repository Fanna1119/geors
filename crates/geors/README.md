# geors

A fast, lightweight geocoder for OpenStreetMap data, as a Rust library.
geors builds memory-mapped indexes, one per country, from `.osm.pbf` extracts.
Use them for text search (with typeahead), reverse geocoding, nearest places,
lookups by OSM id and area containment. You don't need the HTTP server.

```toml
[dependencies]
geors = { version = "0.1", features = ["import"] }
```

```rust,no_run
use geors::import::{ImportOptions, import};
use geors::{Geocoder, GeocoderConfig, LonLat, Reverse, Search};

// Once: build the indexes (or use `geors import` from the CLI).
import("liechtenstein-latest.osm.pbf", "data", &ImportOptions::default())?;

let geocoder = Geocoder::open("data", GeocoderConfig::default())?;
for hit in geocoder.search(&Search::text("Städtle 1, Vaduz").limit(3))? {
    println!("{:?} {:?}", hit.place.street, hit.place.center);
}
let here = geocoder.reverse(&Reverse::at(LonLat::new(9.5215, 47.1412)))?;
# Ok::<(), geors::Error>(())
```

A `Geocoder` is `Send + Sync`: open it once and share it between threads or
in a web server's state. Queries are blocking calls, so in async code wrap
them in `spawn_blocking`. `Geocoder::reload_if_changed` picks up new imports
without interrupting queries that are already running.

## Features

| feature   | what it adds                                                        |
|-----------|---------------------------------------------------------------------|
| (default) | open data directories and query them                                |
| `geojson` | results as GeoJSON features                                         |
| `import`  | import `.osm.pbf` files and Nominatim/Photon dumps (no network code) |
| `update`  | registered sources, Geofabrik/URL downloads, replication diff updates |

## Examples

```sh
cargo run -p geors --features import --example import -- liechtenstein-latest.osm.pbf data
cargo run -p geors --example search -- data "Städtle 1, Vaduz"
```

The `geors` command line and HTTP server (`geors-cli`, `geors-api`) are
built on this crate. The other `geors-*` crates are internal. Their APIs
can change in any release.

License: MIT
