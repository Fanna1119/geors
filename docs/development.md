# Development

[README](../README.md) · [API](api.md) · [Configuration](configuration.md) · [Updates](updates.md) · [Benchmarks](benchmarks.md) · [Architecture](architecture.md) · [Development](development.md)

```sh
cargo test --workspace     # unit and end-to-end tests (no downloads needed)
cargo clippy --workspace --all-targets
RUST_LOG=geors=debug cargo run --release -- serve
```

## Benchmarks

Results are in [benchmarks.md](benchmarks.md). `bench/` generates queries from the data itself, so they work for any extract:

```sh
# Load: throughput and latency per query mix (needs wrk), plus server memory
bench/run.sh data 20s 32

# Quality: is the intended place ranked first?
geors export --data data --every 50 de | bench/accuracy.py http://127.0.0.1:2322 300

# Same-named places in two countries (city exports of both partitions)
bench/ambiguity.py http://127.0.0.1:2322 de.jsonl at.jsonl 200
```

## Static binary

The only native code is `ring` (TLS for downloads), which builds its own C and
assembly and supports musl. A fully static Linux binary therefore works:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Docker

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
