# syntax=docker/dockerfile:1.7

# RTRT runtime image. Multi-stage so the final layer doesn't ship the Rust toolchain.
#
# Build all three binaries with the default feature set (no embeddings/llm-compress
# bloat; those features need an ONNX model download at runtime). Users that want
# the heavy features should build from source with `--features embeddings,llm,
# llm-compress` and copy the binaries themselves.
#
# Usage:
#   docker build -t kernalix7/rtrt:dev .
#   docker run --rm -v $HOME/.rtrt:/data/.rtrt kernalix7/rtrt:dev rtrt --version
#   docker run --rm -v $HOME/.rtrt:/data/.rtrt kernalix7/rtrt:dev rtrt-mcp \
#     --memory /data/.rtrt/memory.sqlite
#   docker run --rm -p 3111:3111 -e RTRT_DASHBOARD_BIND=0.0.0.0:3111 \
#     kernalix7/rtrt:dev rtrt-dashboard

# ---------- build stage ----------
FROM rust:1.85-slim-bookworm AS build

# rusqlite bundled SQLite needs a C compiler.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Cache deps before sources by copying just manifests first.
COPY Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml ./
COPY crates ./crates
COPY scripts ./scripts

ENV CARGO_TERM_COLOR=always
RUN cargo build --release --workspace \
        --bin rtrt --bin rtrt-mcp --bin rtrt-dashboard

# ---------- runtime stage ----------
FROM debian:bookworm-slim AS runtime

# rustls-tls means we don't need OpenSSL, but reqwest still wants CA roots.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -g 10001 rtrt \
    && useradd -u 10001 -g rtrt -d /data -s /sbin/nologin -M rtrt \
    && install -d -o rtrt -g rtrt /data /data/.rtrt

COPY --from=build /src/target/release/rtrt           /usr/local/bin/rtrt
COPY --from=build /src/target/release/rtrt-mcp       /usr/local/bin/rtrt-mcp
COPY --from=build /src/target/release/rtrt-dashboard /usr/local/bin/rtrt-dashboard
COPY LICENSE README.md /usr/share/doc/rtrt/

USER rtrt
WORKDIR /data
ENV HOME=/data \
    RTRT_DASHBOARD_BIND=0.0.0.0:3111

EXPOSE 3111

ENTRYPOINT []
CMD ["rtrt", "--help"]
