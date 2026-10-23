# Rust library

[README](../README.md) · [API](api.md) · [Configuration](configuration.md) · [Updates](updates.md) · [Benchmarks](benchmarks.md) · [Architecture](architecture.md) · [Development](development.md) · [Library](library.md)

The `geors` crate gives you the geocoder without the HTTP server. It opens
the data directory `geors import` writes and answers the same queries as
the [HTTP API](api.md): search, reverse, nearest, lookup and contains. The
HTTP server and the CLI are built on it.

- [Setup](#setup)
- [Building the data](#building-the-data)
- [Opening a geocoder](#opening-a-geocoder)
- [Search](#search)
- [Reverse geocoding](#reverse-geocoding)
- [Nearest places](#nearest-places)
- [Lookup by OSM id](#lookup-by-osm-id)
- [Areas containing a point](#areas-containing-a-point)
- [Reading results](#reading-results)
- [GeoJSON output](#geojson-output)
- [Errors](#errors)
- [Sharing a geocoder and hot reload](#sharing-a-geocoder-and-hot-reload)
- [Importing and updating from code](#importing-and-updating-from-code)
- [Inspecting a data directory](#inspecting-a-data-directory)

## Setup

```toml
[dependencies]
geors = { git = "https://github.com/Fanna1119/geors" }
# or, with features:
# geors = { git = "https://github.com/Fanna1119/geors", features = ["geojson", "import"] }
```

| feature   | adds                                                                  |
|-----------|-----------------------------------------------------------------------|
| (default) | open data directories and query them                                  |
| `geojson` | `geors::geojson`: results as GeoJSON features                         |
| `import`  | `geors::import`: import `.osm.pbf` files and Nominatim/Photon dumps (no network code) |
| `update`  | `geors::update`: registered sources, downloads, replication diff updates (implies `import`) |

The `geors-*` crates (`geors-core`, `geors-index`, ...) are internal. Use
the types `geors` re-exports; the internal APIs can change in any release.

API reference: `cargo doc -p geors --all-features --open`.

## Building the data

The geocoder reads a data directory with one partition per country. Build
it with the CLI, or from code with the `import` feature
([below](#importing-and-updating-from-code)):

```sh
geors import geofabrik:europe/liechtenstein --data data
```

## Opening a geocoder

```rust
use geors::{Geocoder, GeocoderConfig};

let geocoder = Geocoder::open("data", GeocoderConfig::default())?;
```

`open` memory-maps every partition, so it is fast and the index stays in
the OS page cache instead of the heap. Open one `Geocoder` and share it
([below](#sharing-a-geocoder-and-hot-reload)).

`GeocoderConfig` holds the same settings as the `[ranking]` and
`[synonyms]` sections of the [server config](configuration.md), plus the
countries to load:

```rust
use geors::synonyms::{SynonymConfig, Synonyms};
use geors::{GeocoderConfig, RankingConfig};

let config = GeocoderConfig {
    // Only these partitions (lowercase ISO codes); empty: all.
    countries: vec!["li".into(), "ch".into()],
    ranking: RankingConfig {
        location_bias: 0.8,
        ..Default::default()
    },
    synonyms: Synonyms::load(&SynonymConfig {
        languages: vec!["de".into(), "en".into()],
        ..Default::default()
    })
    .map_err(geors::Error::InvalidRequest)?,
};
```

`RankingConfig` and `SynonymConfig` implement `serde::Deserialize`, so you
can read them from your own config file.

## Search

Requests are builders. Each has defaults, and the geocoder validates the
request when it runs it.

**Coordinates are `LonLat::new(lon, lat)`: longitude first**, as in
GeoJSON.

```rust
use geors::{Layer, LonLat, Search};

// Text search, search-as-you-type: "Städtle 1, Vad" finds Vaduz.
let hits = geocoder.search(&Search::text("Städtle 1, Vaduz"))?;

// With every option.
let hits = geocoder.search(
    &Search::text("bahnhof")
        .near(LonLat::new(9.5209, 47.1410)) // rank places near this point higher
        .radius_m(5_000.0)                  // and only within 5 km of it
        .countries(["LI"])                  // ISO codes, any case
        .layers([Layer::Poi, Layer::Street])
        .limit(5)                           // default 10
        .autocomplete(false),               // default true: the last word may be a prefix
)?;
```

| method | default | |
|---|---|---|
| `Search::text(q)` | | text query |
| `.near(point)` | none | rank results near `point` higher (how much: `RankingConfig::location_bias`) |
| `.radius_m(m)` | none | only results within `m` metres of `.near` (needs a point) |
| `.bbox(bbox)` | none | only results intersecting the box |
| `.countries(codes)` | all loaded | only these countries |
| `.layers(layers)` | all | only these layers |
| `.tags(filter)` | none | OSM tag filter, see below |
| `.limit(n)` | 10 | at most `n` results |
| `.autocomplete(on)` | `true` | treat the last word as a prefix unless the query ends with a space |

Searches don't need text:

```rust
use geors::{BBox, Layer, LonLat, Search, TagFilter};

// Cafés within 500 m, nearest first.
let cafes = geocoder.search(
    &Search::around(LonLat::new(9.5209, 47.1410))
        .radius_m(500.0)
        .tags(TagFilter::parse(["amenity:cafe"]).map_err(geors::Error::InvalidRequest)?),
)?;

// The most important cities in a box.
let cities = geocoder.search(
    &Search::within(BBox::new(9.47, 47.05, 9.64, 47.27))
        .layers([Layer::City])
        .limit(20),
)?;
```

`TagFilter::parse` takes the same values as the HTTP `osm_tag` parameter:
`amenity` (key), `amenity:cafe` (key and value), `:cafe` (value), and the
same with a leading `!` to exclude (`!amenity:parking`).

Layers, from most to least specific: `House`, `Poi`, `Street`, `Locality`,
`District`, `City`, `County`, `State`, `Country`. `"city".parse::<Layer>()`
also works.

## Reverse geocoding

What is at a point. By default it returns one result within 1 km, among
houses, POIs, streets and localities (`geors::request::REVERSE_LAYERS`).
Distances to streets are measured to their line, not their centre.

```rust
use geors::{Layer, LonLat, Reverse};

let here = geocoder.reverse(&Reverse::at(LonLat::new(9.5215, 47.1412)))?;

let streets = geocoder.reverse(
    &Reverse::at(LonLat::new(9.5215, 47.1412))
        .radius_m(200.0)
        .layers([Layer::Street])
        .limit(3),
)?;
```

`Reverse` also has `.countries(...)` and `.tags(...)`.

## Nearest places

The exact nearest places at any distance (reverse stops at its radius).
Defaults: 5 results, all layers.

```rust
use geors::{Layer, LonLat, Nearest};

let nearest = geocoder.nearest(
    &Nearest::to(LonLat::new(9.5215, 47.1412))
        .layers([Layer::Poi])
        .max_distance_m(2_000.0)
        .limit(10),
)?;
for hit in &nearest {
    println!("{:?} {:.0} m", hit.place.name, hit.distance_m.unwrap_or_default());
}
```

## Lookup by OSM id

```rust
use geors::PlaceRef;

// Kunstmuseum Liechtenstein (a way) and the town node of Vaduz.
let ids: Vec<PlaceRef> = ["W28712148", "N1932181216"]
    .iter()
    .map(|s| s.parse())
    .collect::<Result<_, _>>()
    .map_err(geors::Error::InvalidRequest)?;
// Empty country list: look everywhere. ["LI"] is faster for large data.
let places = geocoder.lookup(&ids, &[] as &[&str])?;
```

Ids are `N` (node), `W` (way) or `R` (relation) plus the OSM id, or `P` plus
a Nominatim `place_id` for imported dumps. Results come back in request
order; unknown ids are skipped. A street merged from several OSM ways is
found by the id of any of them.

## Areas containing a point

Areas whose polygon contains the point, most specific first (district,
city, county, state, country):

```rust
use geors::{Contains, Layer, LonLat};

let areas = geocoder.contains(&Contains::at(LonLat::new(9.5215, 47.1412)))?;
let cities = geocoder.contains(&Contains::at(LonLat::new(9.5215, 47.1412)).layers([Layer::City]))?;
```

Only area layers (`Locality` and up) are allowed here.

## Reading results

Every query returns `Vec<Hit>`:

```rust
for hit in &hits {
    let p = &hit.place;
    println!("{:?} {}:{} at {:?}", p.layer, p.osm_key, p.osm_value, p.center);

    // Name in a language, falling back to the default name.
    let name = p.localized_name(Some("de"));
    // Houses have no name, use their street and number.
    let label = name.map(str::to_string).or_else(|| {
        Some(format!("{} {}", p.street.as_deref()?, p.housenumber.as_deref()?))
    });

    // Address from the admin hierarchy, in a language.
    let address = hit.address(Some("de"));
    println!("{label:?}, {:?} {:?}, {}", p.postcode, address.city, address.country_code);

    // Distance from the request's point (near / around / reverse / nearest).
    if let Some(m) = hit.distance_m {
        println!("{m:.0} m away");
    }
}
```

| field | |
|---|---|
| `hit.place` | the `Place`: `name`, `names` (per language), `alt_names`, `layer`, `osm_type`, `osm_id`, `osm_key`, `osm_value`, `housenumber`, `street`, `postcode`, `center`, `extent`, `importance`, `country_code` |
| `hit.address(lang)` | `Address { district, city, county, state, country, country_code }` |
| `hit.distance_m` | metres from the request's point, if it had one |
| `hit.score` | ranking score (only comparable within one response) |
| `hit.shape()` | full geometry: `Shape::Point`, `Shape::Lines` (streets) or `Shape::Polygons` (areas) |

`Place` implements `serde::Serialize`.

## GeoJSON output

With the `geojson` feature, results render exactly like the HTTP API's
responses ([format](api.md#response-format)):

```rust
use geors::geojson::{Output, collection};

let out = Output {
    lang: Some("de".into()), // names and address in German
    full_geometry: true,     // lines and polygons instead of the centre point
};
let json = collection(&hits, &out).to_string();
```

`geors::geojson::feature(&hit, &out)` renders a single result.

## Errors

Everything returns `geors::Result<T>`, with one error type:

```rust
use geors::{Error, Search};

match geocoder.search(&Search::text("Vaduz").countries(["XX"])) {
    Ok(hits) => println!("{} results", hits.len()),
    // Bad parameters: safe to show to a user (an HTTP 400).
    Err(Error::InvalidRequest(msg)) => eprintln!("bad request: {msg}"),
    Err(Error::CountryNotLoaded { requested, loaded }) => {
        eprintln!("{requested} is not loaded, only {}", loaded.join(", "))
    }
    // Unreadable data, failed imports: your fault, not the caller's.
    Err(e) => eprintln!("error: {e}"),
}
```

`InvalidRequest` covers things like a radius without a point, coordinates
out of range, a limit of 0 or an invalid country code. `Error` is
`#[non_exhaustive]`, so keep a catch-all arm.

## Sharing a geocoder and hot reload

A `Geocoder` is `Send + Sync`. Open it once and share it with an `Arc` or
your web framework's state. Queries are blocking: they are CPU-bound and
read memory-mapped files. In async code, run them on a blocking thread so
they don't stall the runtime.

An [axum](https://docs.rs/axum) handler:

```rust
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use geors::geojson::{Output, collection};
use geors::{Error, Geocoder, Search};

#[derive(serde::Deserialize)]
struct Params {
    q: String,
}

async fn search(State(geocoder): State<Arc<Geocoder>>, Query(p): Query<Params>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let hits = geocoder.search(&Search::text(p.q).limit(5))?;
        Ok::<_, Error>(collection(&hits, &Output::default()).to_string())
    })
    .await
    .expect("search panicked");
    match result {
        Ok(json) => ([("content-type", "application/geo+json")], json).into_response(),
        Err(e @ (Error::InvalidRequest(_) | Error::CountryNotLoaded { .. })) => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let geocoder = Arc::new(Geocoder::open("data", Default::default())?);
    let app = axum::Router::new()
        .route("/search", axum::routing::get(search))
        .with_state(geocoder);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

Every import and update bumps `<data>/GENERATION`.
`reload_if_changed` checks it and, if the data changed, opens the new
partitions and swaps them in. Queries already running finish on the old
data. Call it after your own imports, or on a timer to pick up imports
from other processes:

```rust
use std::sync::Arc;
use geors::Geocoder;

let geocoder = Arc::new(Geocoder::open("data", Default::default())?);
let watcher = geocoder.clone();
std::thread::spawn(move || loop {
    std::thread::sleep(std::time::Duration::from_secs(30));
    match watcher.reload_if_changed() {
        Ok(true) => println!("new data loaded"),
        Ok(false) => {}
        Err(e) => eprintln!("reload failed, keeping the old data: {e}"),
    }
});
```

`geocoder.status()` reports what is loaded: the data generation, when it
was loaded, and each partition's country, place count per layer, bounding
box and source.

## Importing and updating from code

With the `import` feature, `geors::import` builds data directories from
local files: `.osm.pbf` extracts and Nominatim/Photon JSON dumps.

```rust
use geors::import::{ImportOptions, import};

let report = import(
    "austria-latest.osm.pbf",
    "data",
    &ImportOptions {
        countries: vec!["AT".into()],     // default: every country in the extract, minus border slivers
        importance: Some("wikimedia-importance.csv.gz".into()), // optional, see configuration.md
        ..Default::default()
    },
)?;
println!("wrote {:?}", report.partitions);
for s in &report.skipped {
    println!("skipped {} ({} places): {}", s.country_code, s.places, s.reason);
}
geocoder.reload_if_changed()?; // if a geocoder on "data" is open
```

| `ImportOptions` | default | |
|---|---|---|
| `name` | from the file name (`austria`) | source name; tells apart partitions of a country several extracts hold (`ru`, `ru@asia`) |
| `countries` | all, minus border slivers | only these countries |
| `all_countries` | `false` | keep border slivers too |
| `default_country` | none | country for places outside every boundary (city extracts) |
| `importance` | none | Nominatim's `wikimedia-importance.csv.gz`, for Wikipedia-based ranking |

An import holds a lock on the data directory, so a second import into the
same directory fails instead of corrupting it. Imports are memory hungry
on large extracts. To change their memory use and threads, call
`geors::import::configure` once before the first import (the CLI's
`--threads`, `--index-memory-mb` and `--node-lookup` flags use it).

The `update` feature adds downloads and diff updates, which is what
`geors import` / `geors update` do:

```rust
use geors::update::{AddOptions, Outcome, UpdateOptions, add_source, update};

// Download, import and register for updates.
add_source("data", "geofabrik:europe/liechtenstein", &AddOptions::default())?;

// Later (e.g. daily): apply replication diffs to every registered source.
for report in update("data", &UpdateOptions::default())? {
    match report.outcome {
        Outcome::Updated { partitions } => println!("{}: updated {partitions:?}", report.name),
        Outcome::UpToDate => {}
        Outcome::Failed(e) => eprintln!("{}: {e}", report.name),
    }
}
```

`geors::update::sources` lists the registered sources and
`remove_source` stops tracking one. See [updates](updates.md) for how
sources and diffs work.

## Inspecting a data directory

`geors::data` works without opening a geocoder. It is what `geors info`,
`geors upgrade` and `geors export` use:

```rust
for p in geors::data::partitions("data")? {
    match p.info {
        Ok(info) => println!("{}: {} places", p.name, info.places),
        Err(e) => println!("{}: unusable: {e}", p.name), // e.g. written by an older version
    }
}

// Every 1000th place of Liechtenstein, with its admin parents.
geors::data::export("data", "li", 1000, |place, parents| {
    println!("{:?} {parents:?}", place.name);
    Ok::<_, geors::Error>(())
})?;
```

`geors::synonyms::normalize("Städtle-Straße")` shows the words the index
sees for a text (`stadtle`, `strasse`). `Synonyms::expand` shows what a
word expands to, as `geors synonyms` does.
