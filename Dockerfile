# syntax=docker/dockerfile:1

ARG YTDLP_VERSION=2026.08.19
# Keep in step with the provider image tag in compose.yaml.
ARG BGUTIL_VERSION=2.0.0
ARG DENO_VERSION=2.9.7
ARG RUST_VERSION=1

FROM denoland/deno:bin-${DENO_VERSION} AS deno

FROM rust:${RUST_VERSION}-bookworm AS build

RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
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

RUN install -d -o bot -g bot /app/data
ENV CACHE_PATH=/app/data/cache.redb
ENV LOG_PATH=/app/data/debug.log

USER bot
WORKDIR /app

ENV RUST_LOG=slap=info,warn

STOPSIGNAL SIGINT

ENTRYPOINT ["/usr/local/bin/slap"]
