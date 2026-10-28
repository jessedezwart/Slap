# syntax=docker/dockerfile:1

ARG YTDLP_VERSION=2026.08.19
# Keep in step with the provider image tag in compose.yaml.
ARG BGUTIL_VERSION=2.0.0
ARG DENO_VERSION=2.9.7
ARG RUST_VERSION=1
ARG CARGO_CHEF_VERSION=0.1.71

FROM denoland/deno:bin-${DENO_VERSION} AS deno

FROM rust:${RUST_VERSION}-bookworm AS chef

RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*

ARG CARGO_CHEF_VERSION
RUN cargo install cargo-chef --locked --version ${CARGO_CHEF_VERSION}

WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build

# Dependencies get their own layer, keyed on the recipe (manifest + lockfile),
# so the registry cache reuses it when only Slap's own source changes.
# No cache mounts here: their contents never reach the exported layer cache.
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked \
 && cp target/release/slap /usr/local/bin/slap

ARG YTDLP_VERSION
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
      amd64) asset=yt-dlp_linux ;; \
      arm64) asset=yt-dlp_linux_aarch64 ;; \
      *) echo "no yt-dlp build for this architecture" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /usr/local/bin/yt-dlp \
      "https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/${asset}"; \
    chmod 0755 /usr/local/bin/yt-dlp; \
    /usr/local/bin/yt-dlp --version

# PO token plugin: without a token, googlevideo cuts streams off with a 403
# after the first MiB. It fetches tokens from the provider in compose.yaml.
ARG BGUTIL_VERSION
RUN mkdir -p /etc/yt-dlp/plugins \
 && curl -fsSL -o /etc/yt-dlp/plugins/bgutil-ytdlp-pot-provider.zip \
      "https://github.com/Brainicism/bgutil-ytdlp-pot-provider/releases/download/${BGUTIL_VERSION}/bgutil-ytdlp-pot-provider.zip"

FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 bot

COPY --from=build /usr/local/bin/yt-dlp /usr/local/bin/yt-dlp
COPY --from=build /etc/yt-dlp/plugins /etc/yt-dlp/plugins
# yt-dlp needs a JS runtime to solve YouTube's player challenges.
COPY --from=deno /deno /usr/local/bin/deno
COPY --from=build /usr/local/bin/slap /usr/local/bin/slap

RUN install -d -o bot -g bot /app/data /app/secrets
ENV CACHE_PATH=/app/data/cache.redb

USER bot
WORKDIR /app

STOPSIGNAL SIGINT

ENTRYPOINT ["/usr/local/bin/slap"]
