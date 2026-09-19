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
- Spotify playlists (metadata imported from Spotify, audio matched on YouTube; setup below)

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

## Commands

Join a voice channel, then use:

- `/play url:<track-or-playlist-link>` — play a track or append a playlist.
- `/play url:<playlist-link> shuffle:true` — shuffle the import before adding it.
- `/shuffle` — shuffle upcoming tracks while keeping the current track in place.
- `/pause` and `/resume` — pause and continue the current track.
- `/stop` — stop playback, clear the queue, cancel pending imports, and disconnect.
- `/queue` or `/queue page:2` — show the current track and five upcoming tracks per page.

Controls require you to be in the bot's voice channel. A `/play` request from
another channel will not move the bot or interrupt that channel's queue.
Playlist imports inspect at most 500 entries and report skipped unavailable
entries and truncation. Links containing both a video and a playlist import the
playlist; remove the playlist parameter to request only the video. Tracks resolve
their audio URLs when playback approaches, and failures advance to the next track.
The Opus-only source restriction still applies.

The bot joins **self-deafened** and disconnects as soon as it is the only member
left in its voice channel, including while paused or loading a playlist. Other
bots count as members. Leaving clears the queue and cancels pending imports.
No additional permissions or privileged intents are needed: self-deafening uses
Discord's [voice-state update](https://discord.com/developers/docs/events/gateway-events#update-voice-state),
and does not require **Deafen Members**. The existing `GUILD_VOICE_STATES` intent
supplies channel occupancy updates.

Restart the bot after updating to register the new slash commands.

## Spotify playlists

Configure `SPOTIFY_CLIENT_ID`, `SPOTIFY_CLIENT_SECRET`, and
`SPOTIFY_REFRESH_TOKEN` in `.env`, then restart the bot. Spotify is optional;
other sources work without these settings.

1. Create an app in the [Spotify developer dashboard](https://developer.spotify.com/dashboard).
2. Use Spotify's [Authorization Code flow](https://developer.spotify.com/documentation/web-api/tutorials/code-flow)
   to authorize the account whose playlists the bot will import. Register the
   redirect URI in the dashboard and request `playlist-read-private` and
   `playlist-read-collaborative`. Exchange the authorization code for tokens,
   and put the returned **refresh token** in `SPOTIFY_REFRESH_TOKEN`.
   A client-credentials token does not represent a user and is insufficient.
3. Run `/play url:https://open.spotify.com/playlist/PLAYLIST_ID`.
   Add `shuffle:true` to shuffle before enqueueing.

The bot reads playlist pages in order, up to 500 entries, and searches YouTube
for each track's **artist(s) + song name** when playback approaches. It uses the
first search result, so a live version, cover, or other mismatch is possible.
Local files, episodes, removed tracks and entries without artist/title metadata
are skipped. Failed YouTube playback advances to the next queued track.
Full playlist URLs (including localized `intl-...` URLs and share parameters)
are supported; shortened `spotify.link` URLs, albums, and individual Spotify
track links are not.

Spotify [restricts playlist contents](https://developer.spotify.com/documentation/web-api/reference/get-playlists-items)
to playlists the authorized account owns or collaborates on. Arbitrary public
playlists therefore cannot be imported through this integration. Development
apps also require an app owner with Premium and have user allowlist limits; see
[Spotify's development-mode rules](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide).
All Discord users of this bot use the same configured Spotify account.
Access tokens refresh automatically for imports. If Spotify returns a rotated
refresh token it is retained in memory; reauthorize/update `.env` if authorization
fails after a restart.

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
- view channel

The invite also includes the `applications.commands` scope for slash commands.
