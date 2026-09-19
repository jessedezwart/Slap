# syntax=docker/dockerfile:1

# Pin yt-dlp so image builds are reproducible. YouTube breaks extractors often,
# so bump this deliberately rather than leaving it floating:
#   docker build --build-arg YTDLP_VERSION=2026.09.01 .
ARG YTDLP_VERSION=2026.08.19
ARG RUST_VERSION=1

# --- build -------------------------------------------------------------------
FROM rust:${RUST_VERSION}-bookworm AS build

# cmake and a C compiler are needed by audiopus_sys, which builds libopus from
# source. It links statically, so the runtime image needs neither of them —
# nor libopus itself. (The bot never calls the encoder; the dependency comes
# with songbird's `driver` feature whether we use it or not.)
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Cache mounts keep the crate registry and target directory across builds.
# They are not part of the image layer, so the binary is copied out of them.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/rust-music-bot /usr/local/bin/rust-music-bot

# yt-dlp's Linux release is a self-contained binary, so the runtime image needs
# no Python. Fetched here because this stage already has curl.
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

# --- runtime -----------------------------------------------------------------
FROM debian:bookworm-slim

# ca-certificates is not optional: twilight verifies TLS against the system
# trust store, so without it every gateway and googlevideo connection fails.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 10001 bot

COPY --from=build /usr/local/bin/yt-dlp /usr/local/bin/yt-dlp
COPY --from=build /usr/local/bin/rust-music-bot /usr/local/bin/rust-music-bot

USER bot
# dotenvy searches upward from the working directory, so a `.env` mounted at
# /app/.env is picked up automatically. It is deliberately not baked into the
# image — see .dockerignore. `docker run --env-file` / compose's `env_file:`
# work too and take priority (real env vars win over the file).
WORKDIR /app

ENV RUST_LOG=rust_music_bot=info,warn

# The bot shuts down on SIGINT (tokio's ctrl_c). Docker sends SIGTERM by
# default, which would be ignored until the 10s kill timer expired, so ask for
# SIGINT and let the graceful teardown actually run.
STOPSIGNAL SIGINT

ENTRYPOINT ["/usr/local/bin/rust-music-bot"]
