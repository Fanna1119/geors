# Incremental updates: design

Status: proposal, based on measurements (branch `incremental-updates-test`).
Goal (issue #1): apply replication diffs without re-importing the whole
extract. Today `geors update` merges the diffs into the base PBF and then
re-imports everything: Germany 4 min 20 s, Europe 28 min.

## What a diff contains (measured)

Tool: `cargo run --release -p geors-update --example diff_stats -- BASE.osm.pbf`.

One daily Geofabrik diff for Germany (sequence 4933):

|           | created | modified | deleted |
|-----------|--------:|---------:|--------:|
| nodes     |  65,077 |   48,305 |   9,234 |
| ways      |   8,932 |   19,934 |     860 |
| relations |     115 |    1,044 |      38 |

- **Places to rebuild directly:** about 16.5 k searchable elements (3.8 k
  nodes, 11.6 k ways, 1.2 k relations), out of 24.6 M places. Rebuilding
  them is negligible work.
- **Silent reshapes are common:** 39,876 nodes moved, 37,805 of them
  untagged geometry nodes. **2,319 searchable ways** use a moved node but
  are not in the diff themselves (20 % on top of the ways in it). Ignoring
  them until the next full rebuild would leave thousands of stale shapes per
  day, so they must be found: one parallel pass over the base's way blocks
  (as import pass 2: about 12 s for Germany, about 75 s for Europe).
- **Boundaries change every day:** the diff reshapes 4 country borders (level
  2), 19 states / regions, 47 counties, 116 municipalities and districts, and
  54 postcode areas, almost all through a few moved border nodes. "Fall back
  to a full rebuild when a large boundary changes" would rebuild daily, so
  boundary changes must be patched too. Their effect is local: only places
  near the changed border segments can change parent.
- **Merging the diff into the base PBF** (today's first step) takes 74 s for
  Germany, because it rewrites the whole 4.25 GB file. That would be about
  9 minutes for Europe, per diff.

## Design

### 1. The base stays fixed between full rebuilds

Downloaded diffs are not merged into the base PBF. They are kept as a
compact **change store** (all diffs since the last full rebuild, newest
version per element, sorted by type and id). Every lookup of an element
asks the change store first, then the base. The base PBF is only rewritten
by a full rebuild, which applies all pending diffs at once (the existing
merge).

To read elements from the base by id without scanning it, the import keeps
a **block index**: the first and last id of every node / way / relation
block. Looking up a sorted batch of ids then decodes only the blocks that
contain them (the existing `sorted` join does the same for nodes).

### 2. Kept import state

A full import keeps what is needed to rebuild single places:

| State | Why | Size (Germany) |
|---|---|---|
| boundary polygon stores + area metadata (units, levels, extents) | assign parents to patched places | ~370 MB (already memory-mapped files) |
| admin units, locality points | city / district fallback | small |
| block index of the base PBF | read elements by id | < 1 MB |

Street merging needs no extra state: every merged street stores the ids of
all its segments (`merged_ids`), and the text index finds a street by any
segment id.

### 3. Patch partitions (serving)

Each country partition `<cc>` can have a **patch partition** `<cc>.patch/`
(a normal partition written by the existing writer) and a **tombstone list**:
place ids of the main partition that are deleted or replaced. All endpoints
(search, reverse, nearest, lookup, contains) also query the patch partition
and skip tombstoned places; for text search the tombstones become a "not
deleted" filter bitset (the engine already filters by bitset). Hot reload
works as today (generation bump).

Each update rewrites the patch partition from all changes since the last
full rebuild (small: a few hundred thousand places after a month for
Germany), so there is only ever one patch per country.

### 4. From a diff to patched places

1. **Affected places:**
   - created / modified / deleted searchable nodes, ways, relations;
   - ways reshaped by moved nodes (way-block scan, see above), and relations
     whose member ways changed (multipolygons, boundaries);
   - streets: the whole merged street of any changed segment, re-merged
     from all its segments;
   - interpolation lines touching a changed address node;
   - boundaries whose shape changed: their polygon is rebuilt, and places in
     the bounding box of the changed border segments get their parents
     re-assigned (only those whose parents actually change are patched);
   - new, moved or deleted `place=*` nodes: places within the node's
     fallback radius that have no city / district boundary.
2. **Fetch** the elements and coordinates these need (change store, then
   block-indexed base).
3. **Build** the places with the existing import code (classification,
   geometry, hierarchy from the kept and updated boundary polygons, street
   merging).
4. **Write** the patch partition and tombstones, bump the generation.

### 5. Full rebuilds

`geors update --full`, or automatically once a patch partition passes a
share of its main partition (e.g. 5 %) or a maximum age (e.g. 30 days):
merge all pending diffs into the base, re-import, drop patches and the
change store.

## Expected cost per daily diff (estimate)

| Step | Germany | Europe |
|---|---|---|
| download + parse | 6 s | ~40 s |
| way-block scan for moved nodes | ~12 s | ~75 s |
| fetch elements by id | seconds | seconds–1 min |
| build ~20 k places (Europe ~130 k) + hierarchy | seconds | ~10 s |
| write patch partitions | seconds | ~30 s |
| **total** | **~30–60 s** | **~2–3 min** |

versus 4 min 20 s and 28 min for a full re-import (plus 74 s / ~9 min to
merge the diff into the base first).

## Verification

The import is deterministic, so patching can be checked exactly: apply
real diffs by patching, and separately merge them into the base and
re-import; the exports (main + patch, minus tombstones) must be identical.
Run on Liechtenstein, South Africa, Austria and Germany for several days of
diffs; then hourly diffs.

## Milestones

1. **Patch-aware serving:** patch partitions and tombstones in the engine,
   tested with hand-made patches.
2. **Change store and block index:** keep diffs instead of merging them;
   read elements by id from base + changes.
3. **Kept import state:** boundary stores, area metadata, units, localities
   saved with the partition data.
4. **Diff → patch** for direct changes, streets, moved nodes, interpolation;
   verification harness ("patch = rebuild").
5. **Boundary and locality changes** (parent re-assignment).
6. **Policy:** `geors update` patches by default, folds into a full rebuild
   by size / age, `--full` forces one.

Each milestone is usable and testable on its own on small extracts.

## Open decisions

- Thresholds for folding into a full rebuild (5 % / 30 days proposed).
- Disk: kept import state adds ~400 MB for Germany, a few GB for Europe.
- Minutely diffs (planet.openstreetmap.org) would work with the same
  design; Geofabrik publishes daily.
