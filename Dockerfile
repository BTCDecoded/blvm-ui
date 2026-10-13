# syntax=docker/dockerfile:1.7
#
# Commons Node View: one multi-arch image (linux/amd64 + linux/arm64) used for BOTH containers of the Umbrel app.
#
#   web container    `blvm-ui-next`                          (default command: the console)
#   agent container  `blvm-ui-next agent --root /data`      (runs the node; overrides the command)
#
# The BLVM node itself is NOT in the image. The setup walkthrough downloads the official release binary (and checks
# it against the release's checksums.sha256) into /data when the user picks a network.
#
# The web page and the Rust binary build on the build machine's own architecture and the binary is cross-compiled
# for the target, so no QEMU emulation is needed.
#
#   docker buildx build --platform linux/amd64,linux/arm64 -t ghcr.io/btcdecoded/blvm-ui:0.2.0 --push .

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
# Built on Debian 12 (glibc 2.36); it runs on the Ubuntu 24.04 runtime below (glibc 2.39) because glibc is
# backwards compatible. Any C code in dependencies (for example a TLS library) is compiled with the cross gcc.
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
      export "CC_$(echo "$T" | tr '-' '_')=$(cat /gnu)-gcc"; \
    fi; \
    cargo build --release --locked --target "$T"; \
    cp "target/$T/release/blvm-ui-next" /blvm-ui-next

# ---- runtime ----------------------------------------------------------------
# Ubuntu 24.04: the official BLVM release binary needs glibc 2.39 (Debian 12 has 2.36, too old), and the agent
# container runs that binary from this same image.
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /data && chown 1000:1000 /data
WORKDIR /app
# Binary, page and location files sit next to each other so the host finds them without extra settings.
COPY --from=build /blvm-ui-next /app/blvm-ui-next
COPY --from=web /src/web/dist /app/web
COPY --from=geo /geo /app/geo
COPY geo/zone.tab /app/geo/zone.tab

ENV PATH=/app:$PATH \
    HOME=/data \
    BLVM_UI_LISTEN=0.0.0.0:3849 \
    BLVM_UI_STATE_DIR=/data \
    RUST_LOG=blvm_ui_next=info
VOLUME /data
EXPOSE 3849
# UID 1000 (Ubuntu's built-in "ubuntu" user): what Umbrel gives app data folders. Nothing needs root.
USER 1000:1000
# Only the web container answers HTTP; the agent container turns this off in its compose file.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3849/api/status >/dev/null || exit 1
ENTRYPOINT ["blvm-ui-next"]
CMD []
