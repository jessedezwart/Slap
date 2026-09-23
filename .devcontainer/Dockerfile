# syntax=docker/dockerfile:1

# Keep these pins aligned with the production Dockerfile.
ARG YTDLP_VERSION=2026.08.19
ARG BGUTIL_VERSION=2.0.0
ARG DENO_VERSION=2.9.7

FROM denoland/deno:bin-${DENO_VERSION} AS deno

FROM mcr.microsoft.com/devcontainers/rust:1-bookworm
ARG YTDLP_VERSION
ARG BGUTIL_VERSION

# audiopus_sys builds libopus from source with CMake.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake ca-certificates \
 && rm -rf /var/lib/apt/lists/*

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

# PO token plugin; start the provider as described in the README.
RUN mkdir -p /etc/yt-dlp/plugins \
 && curl -fsSL -o /etc/yt-dlp/plugins/bgutil-ytdlp-pot-provider.zip \
      "https://github.com/Brainicism/bgutil-ytdlp-pot-provider/releases/download/${BGUTIL_VERSION}/bgutil-ytdlp-pot-provider.zip"

COPY --from=deno /deno /usr/local/bin/deno

USER vscode
RUN rustup component add rustfmt clippy
