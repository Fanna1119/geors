# HTTP API

[README](../README.md) · [API](api.md) · [Configuration](configuration.md) · [Updates](updates.md) · [Benchmarks](benchmarks.md) · [Architecture](architecture.md) · [Development](development.md) · [Library](library.md)

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

# Category filter (Photon syntax): only cafes and bakeries
curl 'localhost:2322/search?q=vaduz&osm_tag=amenity:cafe&osm_tag=shop:bakery'

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

# Which areas contain this point: district < city < county < state < country
curl 'localhost:2322/contains?lat=48.1374&lon=11.5755'

# Prometheus metrics
curl 'localhost:2322/metrics'
```

To try the API on a map, open `examples/geors-map-test.html` in a browser
while geors runs. It covers every endpoint and parameter, draws full
geometry, and shows `/status` partitions as boxes on the map.

## Reference

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
| `osm_tag`      | Category filter (Photon syntax): `amenity`, `amenity:cafe`, `:cafe`; a leading `!` excludes (`!amenity:parking`). Repeat it or separate with commas: places matching any include and no exclude. |
| `limit`        | Defaults to 10, maximum 50 (configurable).                                  |
| `lang`         | Language for names, such as `en` or `de`. Falls back to the default name.   |
| `autocomplete` | Defaults to `true`. Treats the last word as a prefix unless the query ends with a space. |
| `geometry`     | `point` (default) or `full`: lines for streets and rivers, polygons for areas, buildings and boundaries. With `full`, the centre point moves to the `center` property. |

Without `q`, the endpoint returns the places inside the spatial filter. With
`lat`/`lon` they come nearest first; with only a bbox they come most important
first.

### `GET /reverse`

`lat`, `lon` (required), `radius` (default 1000 m), `limit` (default 1),
`layers` (default `house,poi,street,locality`), `osm_tag`, `country`, `lang`,
`geometry`.
Returns the closest places within the radius. For streets, the distance is
measured to the street's line geometry, not to its centre.

### `GET /nearest`

`lat`, `lon` (required), `limit` (default 5), `radius` (optional cap),
`layers`, `osm_tag`, `country`, `lang`, `geometry`. Returns the exact k nearest places at
any distance.

### `GET /lookup`

`osm_ids=N123,W456,R789` (required, up to `max_limit` ids), `country`, `lang`,
`geometry`. Results come back in request order; unknown ids are skipped. A
merged street can be found by the id of any of its OSM way segments. Data
imported from a Nominatim dump can also be looked up by Nominatim
`place_id`: `osm_ids=P123456`.

### `GET /contains`

`lat`, `lon` (required), `layers` (area layers only: `locality`, `district`,
`city`, `county`, `state`, `country`), `country`, `lang`, `geometry`. All
areas whose polygon contains the point, most specific first, e.g.
Marienplatz < Altstadt-Lehel < München < Oberbayern < Bayern < Deutschland.
Uses the stored outlines, simplified by about 1 m (small areas) up to tens
of metres (countries), so a point right on a border can land on either side.
The first request per country builds a small index of its areas (Germany:
under 2 s); later ones take about a millisecond.

### `GET /metrics`

Prometheus text format: per endpoint `geors_requests_total`,
`geors_request_errors_total` (4xx/5xx) and the latency histogram
`geors_request_duration_seconds`; plus `geors_partitions`, `geors_places`
per country, `geors_data_loaded_timestamp_seconds` and `geors_build_info`
(version, data generation).

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
