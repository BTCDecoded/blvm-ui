# syntax=docker/dockerfile:1.7
#
# Multi-arch image (linux/amd64 + linux/arm64) for the console.
# The web page and the Rust binary build on the build machine's own arch and the
# binary is cross-compiled for the target, so no QEMU emulation is needed.
#
#   docker buildx build --platform linux/amd64,linux/arm64 -t ghcr.io/btcdecoded/blvm-ui:0.1.0 --push .

# ---- web page (arch-independent output) ------------------------------------
FROM --platform=$BUILDPLATFORM node:22-bookworm-slim AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# ---- peer location database (DB-IP Country Lite, CC BY 4.0) ----------------
FROM --platform=$BUILDPLATFORM debian:bookworm-slim AS geo
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
# This month's file, else last month's (a new one appears at the start of each month).
# Without either, the console still runs; peers just have no dot on the globe.
RUN mkdir /geo && cd /geo && \
    for m in "$(date -u +%Y-%m)" "$(date -u -d '-1 month' +%Y-%m)"; do \
      curl -fsSL -o db.mmdb.gz "https://download.db-ip.com/free/dbip-country-lite-$m.mmdb.gz" && \
      gunzip db.mmdb.gz && mv db.mmdb dbip-country-lite.mmdb && break; \
    done; ls -la /geo

# ---- Rust binary, cross-compiled for $TARGETARCH ----------------------------
FROM --platform=$BUILDPLATFORM rust:1.88-bookworm AS build
ARG TARGETARCH
ARG BUILDARCH
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-gnu > /triple; echo x86_64-linux-gnu > /gnu ;; \
      arm64) echo aarch64-unknown-linux-gnu > /triple; echo aarch64-linux-gnu > /gnu ;; \
      *) echo "unsupported arch $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    rustup target add "$(cat /triple)"; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends "gcc-$(cat /gnu | tr _ -)" "libc6-dev-$TARGETARCH-cross"; \
      rm -rf /var/lib/apt/lists/*; \
    fi
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY geo/zone.tab ./geo/zone.tab
RUN set -eux; \
    T="$(cat /triple)"; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      export "CARGO_TARGET_$(echo "$T" | tr 'a-z-' 'A-Z_')_LINKER=$(cat /gnu)-gcc"; \
    fi; \
    cargo build --release --locked --target "$T"; \
    cp "target/$T/release/blvm-ui-next" /blvm-ui

# ---- runtime ----------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /data && chown 1000:1000 /data
WORKDIR /app
COPY --from=build /blvm-ui /app/blvm-ui
COPY --from=web /src/web/dist /app/web
COPY --from=geo /geo /app/geo
COPY geo/zone.tab /app/geo/zone.tab

ENV BLVM_UI_LISTEN=0.0.0.0:3849 \
    BLVM_UI_STATE_DIR=/data \
    RUST_LOG=blvm_ui_next=info
VOLUME /data
EXPOSE 3849
USER 1000:1000
ENTRYPOINT ["/app/blvm-ui"]
