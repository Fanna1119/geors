# Benchmarks

[README](../README.md) · [API](api.md) · [Configuration](configuration.md) · [Updates](updates.md) · [Benchmarks](benchmarks.md) · [Architecture](architecture.md) · [Development](development.md)

## Test hardware

All numbers come from one laptop, not a server:

| | |
|---|---|
| Machine | MacBook Pro, Apple M1 Pro: 8 cores (6 performance + 2 efficiency), 16 GB RAM, internal SSD |
| OS | macOS 26 |
| Docker tests | Docker Desktop VM with 8 CPUs and 7.7 GB, limited per test with `--memory` / `--cpus` |

## Import

Imports keep little on the heap. Node coordinates, boundary polygons and the
finished index are memory-mapped files; sort keys, street segments and the
spatial index are sorted on disk and streamed. The OS can drop those pages
whenever it needs the memory, so a fast SSD matters more than lots of RAM:
South Africa imports within 256 MB, and Germany peaks at 1.0 GB (it was
4.0 GB) while taking no longer. Europe peaks at 5.9 GB (it was 10 GB):
structures that grow with the whole continent (relations, boundary member
ways, per-thread point-in-polygon caches) still dominate there. Import
times include writing the index.

> **Import memory is not serving memory.** The "Import memory" column is
> what *building* the index takes, once per import or update. Serving the
> finished index needs far less: all of Europe runs in about 230 MB at idle
> and 380 MB under load, Asia + Europe + North America in 650 MB and 840 MB
> (see below). You can build the index on a larger
> machine and serve it from a small one. High import memory for continent
> extracts is a known limitation that I'm still working on (see
> [#2](https://github.com/Fanna1119/geors/issues/2)).

| Extract | PBF | Places | Import | Import memory¹ | Index on disk |
|---|---|---|---|---|---|
| Liechtenstein | 3 MB | 16 k | 0.5 s | ~70 MB | 4 MB |
| Austria | 774 MB | 3.1 M | 34 s | 0.5 GB | 0.8 GB |
| South Africa | 402 MB | 525 k³ | 11 s | 0.3 GB | 147 MB |
| Germany | 4.6 GB | 24.6 M | **4 min 20 s** | **1.0 GB** | 5.6 GB |
| Europe (53 countries) | 33 GB | 150 M | **28 min 24 s** | **5.9 GB**² | 27 GB |

¹ Peak memory footprint (heap) while importing, not while serving. Node
coordinates and scratch files are memory-mapped and evictable. Peak extra
disk during import is about the size of the finished index.
² Measured with `bench/import-stats.sh` (default country rules). Peak swap
in use 1.7 GB; peak disk during the import 62 GB (index plus scratch files).
³ Including 42 k interpolated house numbers.

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

## Serving

**Serving Asia + Europe + North America** (105 partitions, 203 M places,
46 GB index) on the same laptop, 32 concurrent connections, queries sampled
from every 500th place (`EVERY=500 bench/run.sh data 20s 32`). The index is
almost three times the laptop's RAM:

| Query mix | req/s | p50 | p99 |
|---|---|---|---|
| full address | 1,379 | 20 ms | 280 ms |
| autocomplete prefix | 206 | 141 ms | 436 ms |
| with typos | 65 | 457 ms | 1.6 s |
| name + location bias | 634 | 43 ms | 449 ms |
| category within 2 km radius | 1,554 | 18 ms | 66 ms |
| reverse | 37,093 | 0.8 ms | 2.9 ms |
| 10 nearest | 20,305 | 1.5 ms | 4.2 ms |

- **Memory:** **654 MB** physical footprint at start and **836 MB** after
  the load test (Europe alone: 232 MB and 380 MB). For comparison, Photon's
  README recommends "at least 64GB RAM" for its planet-wide database (about
  95 GB on disk). These three continents hold most of OSM's addresses, but
  not all.
- **Page cache:** RSS reached 7.1 GB. These are cached index pages, which
  the OS drops under pressure.
- **Errors:** none.
- **Spatial queries** (reverse, nearest) are as fast as on a single country.
- **Text search** depends on how much of the index stays cached. Queries
  without `country=` search every partition that can match. Ranking reads
  one importance value per match from `importance.bin` (0.8 GB for all 105
  partitions), so that file stays cached. Before it existed, reading
  `places.bin` (8.1 GB) for every match made prefix searches 5x slower.
  Searches with a location read each match's centre from `centers.bin`
  (1.6 GB) for the distance: reading `places.bin` instead, they reached
  99–205 req/s depending on the run.
- **Typos:** words are first matched with one typo, with two (words of 8+
  letters) only if that finds nothing. Typo searches remain the slowest:
  every misspelled word expands to all similar words in every partition.
- **Before #18–#25:** on the same data, full address 143 req/s, prefix 17,
  typos 23, location bias 87 (sampling every 20th place).
- **Single client:** a lone search uses all cores, one partition per core.
  Under load, each search uses one core: splitting searches when all cores
  are busy only adds overhead (measured: 20 % fewer req/s at 32 connections).
- **Load generator:** wrk ran on the same 8 cores as the server.

### Germany + Austria + Liechtenstein + Andorra

An earlier measurement, before the search changes of #18–#25 (on Germany +
Austria + Switzerland + Liechtenstein they made full-address search about
3x and prefix search about 2.4x faster).

27.7 M places, 32 concurrent connections (`bench/run.sh`):

| Query mix | req/s | p50 | p99 |
|---|---|---|---|
| full address ("Hauptstraße 5 Wien") | 2,163 | 12 ms | 58 ms |
| autocomplete prefix (1–6 letters) | 733 | 35 ms | 654 ms |
| with typos | 537 | 47 ms | 868 ms |
| name + location bias | 1,262 | 21 ms | 330 ms |
| category within 2 km radius | 815 | 34 ms | 144 ms |
| reverse | 34,733 | 0.8 ms | 8 ms |
| 10 nearest | 20,892 | 1.4 ms | 8 ms |

Server memory (Germany + Austria + Liechtenstein + Andorra): **18 MB physical
footprint at start, ~150 MB under load**.
RSS grows to a few GB as the OS caches index pages; those pages are clean and
the OS drops them under memory pressure.

## Search quality

Germany and Austria (`bench/accuracy.py`, `bench/ambiguity.py`):

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
