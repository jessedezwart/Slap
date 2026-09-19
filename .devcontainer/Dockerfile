# syntax=docker/dockerfile:1
FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

# Keep this pin aligned with the production Dockerfile.
ARG YTDLP_VERSION=2026.08.19

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

USER vscode
RUN rustup component add rustfmt clippy
