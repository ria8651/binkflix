# syntax=docker/dockerfile:1.7
# Multi-stage build for the Dioxus fullstack app.
# Builder produces the server binary + bundled web assets via `dx bundle`;
# runtime is a slim Debian image with just the binary + vendored static files.

FROM rust:1-trixie AS builder
RUN rustup target add wasm32-unknown-unknown \
 && curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash

WORKDIR /src

# Pin dioxus-cli to the exact `dioxus` version locked in Cargo.lock — dx
# refuses to build when the CLI and library disagree (e.g. dx 0.7.9 against
# dioxus 0.7.5). Copy lockfiles first so this layer caches until Cargo.lock
# actually moves.
COPY Cargo.toml Cargo.lock ./
RUN DIOXUS_VER=$(awk '/^name = "dioxus"$/ { getline; gsub(/[" ]/, "", $3); print $3; exit }' Cargo.lock) \
 && cargo binstall -y dioxus-cli@${DIOXUS_VER}

COPY . .

# Build identity baked into both the server binary and the WASM bundle (read
# via `option_env!` — see `BUILD_ID` in src/types.rs), so playback telemetry
# can flag a stale cached frontend. Resolved in the build RUN below, in order:
#   1. an explicit `--build-arg BINKFLIX_BUILD_ID=...` (override), else
#   2. the git short SHA — the compose build passes
#      `BUILDKIT_CONTEXT_KEEP_GIT_DIR=1`, which restores `.git` for the git-URL
#      context (it overrides the `.dockerignore` .git entry), else
#   3. empty (build-arg unset and no .git) — telemetry records an empty id.
ARG BINKFLIX_BUILD_ID

# Cache mounts: cargo registry/git indices, the target dir, and dx's tool cache
# (wasm-bindgen-cli, esbuild, tailwind etc. fetched on first run). Artifacts
# live in target/dx/... which is a cache mount, so we copy them out to /out
# afterwards so the next stage can COPY --from=builder them.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target,id=binkflix-target \
    --mount=type=cache,target=/root/.cache/dioxus \
    git config --global --add safe.directory /src 2>/dev/null || true; \
    export BINKFLIX_BUILD_ID="${BINKFLIX_BUILD_ID:-$(git rev-parse --short HEAD 2>/dev/null)}"; \
    echo "binkflix build id: ${BINKFLIX_BUILD_ID:-<empty>}"; \
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

# Dioxus bundle (server binary + public/ web assets). The server binary also
# hosts one-shot subcommands like `import-jellyfin` and `cleanup`, exposed on
# PATH as `binkflix` via the symlink below — e.g.
# `docker exec <container> binkflix import-jellyfin /data/jf.db`.
COPY --from=builder /out/web/ /app/
RUN ln -s /app/server /usr/local/bin/binkflix
# Vendored static files the server serves directly via ServeDir("assets/..."):
# the JASSUB worker/wasm and the web fonts, which need stable unhashed URLs so
# can't go through Dioxus's asset pipeline. See src/server/mod.rs.
COPY --from=builder /src/assets /app/assets

ENV BINKFLIX_DB=/data/binkflix.db \
    BINKFLIX_BIND=0.0.0.0:9356 \
    BINKFLIX_LOG_DIR=/data/logs

EXPOSE 9356
VOLUME ["/data"]

CMD ["/app/server"]
