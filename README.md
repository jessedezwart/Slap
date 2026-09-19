# rust-music-bot

A Discord music bot that plays audio **without transcode** using yt-dlp and songbird. Designed to be highly efficient.

### Why not use Songbird's built-in YouTube source?

First of all, this is an educational exercise. Goal is managing data efficiently across different services without blocking threads, handling disconnects across the pipeline. And seeing how performant I could make it.

There are a few additional advantages:
- Controlling yt-dlp means we can set to use Opus-in-WebM, which is the only format that can be passed through to Discord without transcoding.
- Songbird loads the entire track into memory. This has disadavantages for ram usage and startup time. And since the complete track needs to be loaded on an OS-thread, it can block the complete thread.
- Songbird doesn't cache yt-dlp metadata. Not that big of a deal, but means additional stalling of an OS-thread.
- Since I only delegate the playback, I have more control over the failure-handling. I think Songbird orphans instead of kills a yt-dlp process when yt-dlp fails for example.

## Sources

Some supported sources:

- YouTube
- SoundCloud
- Bandcamp

Full list is here: https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md

## Architecture

1. yt-dlp resolves a link to YouTube's Opus-in-WebM rendition.
1. The bot fetches the WebM and demuxes it to Opus frames.
1. The frames are passed to songbird for playback.

## Docker

Set the token in .env and run:

```sh
docker compose up
```

### Notes

yt-dlp's version is pinned by the `YTDLP_VERSION` build argument. YouTube
breaks extractors regularly, so rebuild with a newer one when resolution starts
failing:

```sh
docker build --build-arg YTDLP_VERSION=2026.09.01 -t rust-music-bot .
```

## Development container

With Docker running and the VS Code Dev Containers extension installed, open
this repository and run **Dev Containers: Reopen in Container**. The container
includes Rust, rustfmt, Clippy, CMake, yt-dlp, and Rust editor extensions.

Copy `.env.example` to `.env` if you have not already done so, then set
`DISCORD_TOKEN` in `.env`. In the container terminal:

```sh
cargo run
cargo test
```

## Configuration

All configuration settings are documented in `.env.example`. Copy that file to `.env`.

## Notes on behaviour

**Disconnects drop everything.** Each guild owns one `CancellationToken`; every
yt-dlp child, fetch task and buffer holds a descendant of it. Any voice
disconnect — dropped connection, kick, channel move, shutdown — cancels that one
token and the entire pipeline for that guild goes away.

## Diagnostics

Run with `RUST_LOG=rust_music_bot=debug` to see buffer depth and stream lifecycle.
The line to watch for is:

`audio buffer ran dry: playback may have stuttered`

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

## Invite

[Invite the bot](https://discord.com/oauth2/authorize?client_id=881107137990918166&scope=bot%20applications.commands&permissions=3146752)

Replace `client_id` with your bot's client ID. The permissions integer is the minimum needed to join a voice channel and respond to slash commands:

- connect
- speak
- use slash commands
