# syntax=docker/dockerfile:1
#
# Small runtime image: the static-ish geors binary on distroless (no shell,
# non-root). Data lives in the /data volume.
#
#   docker build -t geors .
#   docker run -v $PWD/data:/data -p 2322:2322 --memory 256m --cpus 1 geors
#   docker run -v $PWD/data:/data -v $PWD/osm:/osm geors import /osm/x.osm.pbf

FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin geors \
    && cp target/release/geors /geors

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /geors /usr/local/bin/geors
ENV GEORS_DATA=/data \
    GEORS_BIND=0.0.0.0:2322
VOLUME /data
EXPOSE 2322
ENTRYPOINT ["/usr/local/bin/geors"]
CMD ["serve"]
