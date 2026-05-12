# syntax=docker/dockerfile:1.7
# Multi-stage build for the Dioxus fullstack app.
# Builder produces the server binary + bundled web assets via `dx bundle`;
# runtime is a slim Debian image with just the binary + vendored static files.

FROM rust:1-trixie AS builder
RUN rustup target add wasm32-unknown-unknown \
 && curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash \
 && cargo binstall -y dioxus-cli

WORKDIR /src
COPY . .

# Cache mounts: cargo registry/git indices, the target dir, and dx's tool cache
# (wasm-bindgen-cli, esbuild, tailwind etc. fetched on first run). Artifacts
# live in target/dx/... which is a cache mount, so we copy them out to /out
# afterwards so the next stage can COPY --from=builder them.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target,id=binkflix-target \
    --mount=type=cache,target=/root/.cache/dioxus \
    dx bundle --platform web --release \
 && mkdir -p /out \
 && cp -r /src/target/dx/binkflix/release/web /out/web

FROM debian:trixie-slim AS runtime
# intel-media-va-driver-non-free lives in the non-free component; enable it on
# the default deb822 sources before installing.
RUN sed -i 's/^Components: main$/Components: main non-free non-free-firmware/' \
        /etc/apt/sources.list.d/debian.sources \
 && apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    intel-media-va-driver-non-free vainfo \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Dioxus bundle (server binary + public/ web assets).
COPY --from=builder /out/web/ /app/
# Vendored static files the server serves directly via ServeDir("assets/..."):
# JASSUB worker/wasm and player.js, which need stable unhashed URLs so can't
# go through Dioxus's asset pipeline. See src/server/mod.rs.
COPY --from=builder /src/assets /app/assets

ENV BINKFLIX_DB=/data/binkflix.db \
    BINKFLIX_BIND=0.0.0.0:9356 \
    BINKFLIX_LOG_DIR=/data/logs

EXPOSE 9356
VOLUME ["/data"]

CMD ["/app/server"]
