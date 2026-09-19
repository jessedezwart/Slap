# rust-music-bot

A Discord music bot that plays YouTube audio **without transcoding it**.

yt-dlp resolves a link to YouTube's Opus-in-WebM rendition, this bot fetches those
bytes itself over HTTP, and songbird hands the Opus packets straight to Discord's
voice socket. Nothing decodes or re-encodes audio at any point, so per-track CPU
cost is essentially a memcpy — which is what makes many concurrent streams viable
on a single core.

```
/play <url>
  └─ yt-dlp -j ────────────► direct media URL + headers        (argv, never a shell)
  └─ reqwest ranged GET ───► prefetch buffer (configurable)
                             └─ songbird async adapter
                                └─ symphonia WebM demux
                                   └─ Opus passthrough ──► DAVE encrypt ──► UDP
```

## Requirements

- Rust 1.83+ (songbird 0.6's MSRV)
- **cmake** and a C toolchain — songbird's `driver` feature links libopus
- **yt-dlp** on `PATH`, or set `YTDLP_PATH`
- A Discord bot invited with the `bot` and `applications.commands` scopes

No privileged gateway intents are needed: the bot uses only `GUILDS` and
`GUILD_VOICE_STATES`.

## Running

```sh
export DISCORD_TOKEN=...
cargo run --release
```

`/play` is registered as a global slash command at start-up. Global commands can
take a few minutes to appear in the client the first time.

## Docker

The image carries yt-dlp, so nothing but the token is needed on the host.

```sh
docker build -t rust-music-bot .
docker run --rm --init --env-file .env rust-music-bot
```

`--env-file` is the simplest path. Alternatively mount the file and let the bot
read it itself, which also works with a read-only container:

```sh
docker run --rm --init -v "$PWD/.env:/app/.env:ro" rust-music-bot
```

`--init` is worth keeping: the bot is PID 1 in the container and spawns yt-dlp
children, so a reaper avoids accumulating zombies over a long uptime.

yt-dlp's version is pinned by the `YTDLP_VERSION` build argument. YouTube
breaks extractors regularly, so rebuild with a newer one when resolution starts
failing:

```sh
docker build --build-arg YTDLP_VERSION=2026.09.01 -t rust-music-bot .
```

## Configuration

Everything except the token has a working default. `.env.example` lists every
variable with its default; copy it to `.env` and load it into the environment
(the bot reads the environment, it does not parse `.env` itself).

| Variable | Default | Meaning |
| --- | --- | --- |
| `DISCORD_TOKEN` | *(required)* | Bot token |
| `BUFFER_MS` | `5000` | How much audio to hold ahead of the mixer |
| `PREFILL_RATIO` | `0.5` | Fraction of the buffer filled before playback starts |
| `PREFILL_TIMEOUT_MS` | `10000` | Give up waiting for prefill and start anyway |
| `ASSUMED_BITRATE_BPS` | `160000` | Used to size the buffer when yt-dlp reports no bitrate |
| `YTDLP_PATH` | `yt-dlp` | Binary to spawn |
| `YTDLP_TIMEOUT_MS` | `15000` | Kill yt-dlp after this long |
| `YTDLP_EXTRA_ARGS` | *(empty)* | Extra argv, e.g. `--cookies-from-browser firefox` |
| `WORKER_THREADS` | `1` | Tokio worker threads |
| `RUST_LOG` | `rust_music_bot=info,warn` | Log filter |

`BUFFER_MS` is converted to a byte budget using the track's real bitrate when
yt-dlp reports one. The budget is enforced in bytes by a semaphore, so it holds
regardless of how the network chunks the response.

## Notes on behaviour

**Opus passthrough is conditional.** songbird only passes Opus through while
exactly one track is live *and* its volume is exactly `1.0`. Queued tracks are
paused, so queueing is safe — but adding a `/volume` command would silently turn
the whole pipeline into decode → mix → re-encode. If you add one, say so in its
help text.

**Queued tracks are lazy.** A queued song spawns no yt-dlp process and opens no
socket until songbird's preloader readies it, a few seconds before it is due, so
a deep queue costs nothing and resolved URLs cannot expire while waiting.

**Disconnects drop everything.** Each guild owns one `CancellationToken`; every
yt-dlp child, fetch task and buffer holds a descendant of it. Any voice
disconnect — dropped connection, kick, channel move, shutdown — cancels that one
token and the entire pipeline for that guild goes away.

**Videos without Opus are rejected.** That is deliberate: supporting them would
mean transcoding, which is exactly what this design avoids. The user gets a
message saying so.

## Diagnostics

Run with `RUST_LOG=rust_music_bot=debug` to see buffer depth and stream lifecycle.
The line to watch for is:

```
audio buffer ran dry: playback may have stuttered
```

That is emitted when the mixer asked for bytes and the buffer was empty — the
direct signal that `BUFFER_MS` is too low for the network in question.

## Tests

```sh
cargo test                      # unit + socket-level buffer tests, no network
cargo test -- --ignored         # end-to-end: needs yt-dlp and network access
```

The ignored test resolves a real video and asserts that what songbird ends up
holding is a `CODEC_TYPE_OPUS` track — the precondition the passthrough design
rests on. Point it at another video with `YTDLP_TEST_URL`.
