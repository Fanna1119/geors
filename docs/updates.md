# Keeping data up to date

[README](../README.md) · [API](api.md) · [Configuration](configuration.md) · [Updates](updates.md) · [Benchmarks](benchmarks.md) · [Architecture](architecture.md) · [Development](development.md) · [Library](library.md)

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
whole extract (Germany about 5 minutes, Europe about 30); incremental updates are
tracked in [#1](https://github.com/Fanna1119/geors/issues/1). The base PBF also stays on disk. After the first diff update
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
