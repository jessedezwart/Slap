# Slap

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
docker build --build-arg YTDLP_VERSION=2026.09.01 -t slap .
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
- `/shuffle` — turn shuffle on or off. While on, upcoming tracks and later
  imports play in random order; turning it off restores the queued order.
- `/pause` and `/resume` — pause and continue the current track.
- `/skip` — skip the current track and play the next queued track, or end playback if the queue is empty.
- `/stop` — stop playback, clear the queue, cancel pending imports, and disconnect.
- `/queue` or `/queue page:2` — show the current track and five upcoming tracks per page.

The first successful `/play` response becomes a live player message with the
current title, thumbnail, requester, elapsed and total time, queue size,
shuffle state, and **Pause/Resume**, **Skip**, **Stop**, **Shuffle**, and
**Queue** buttons. Later imports reuse that player, and it is deleted when
playback stops. Queue replies privately with five upcoming tracks; use
`/queue page:2` for further pages. The other buttons, `/skip`, `/stop`, and
`/shuffle` succeed silently because the player shows the result; errors are
private. Unknown track durations show elapsed time only.

The player edits one message per voice session. Progress refreshes every
15 seconds (four scheduled edits per minute while playing), with earlier
updates on playback changes. Event bursts are coalesced and edits are spaced
at least two seconds apart. Unchanged paused/empty views cause no HTTP edits.
Progress-only updates send text without resending the buttons. The updater
uses existing metadata and Songbird's playback position: no extra yt-dlp,
Spotify, artwork or audio requests. Queue reads clone only the current track
and the requested five-track page.

All updates run in an asynchronous task, with no playback locks held during
HTTP or driver queries. Requests have timeouts and transient failures back off.
Stop/disconnect cancels the updater and disables its controls; old buttons
cannot affect a new session. Deleting the player stops its updater; another
`/play` creates a replacement. Edits use the bot token so long sessions are
not limited by the interaction token's lifetime.

Controls require you to be in the bot's voice channel. A `/play` request from
another channel will not move the bot or interrupt that channel's queue.
Playlist imports inspect at most 1000 entries and report skipped unavailable
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

Configure `SPOTIFY_CLIENT_ID` and `SPOTIFY_CLIENT_SECRET` in `.env`, then
restart the bot. Spotify is optional; other sources work without these settings.

1. Create an app in the [Spotify developer dashboard](https://developer.spotify.com/dashboard).
2. Put the client ID and secret in `.env`. This is enough to import **public**
   playlists, up to their first 100 tracks.
3. Optionally, lift that ceiling by also setting `SPOTIFY_REFRESH_TOKEN`. Use
   Spotify's [Authorization Code flow](https://developer.spotify.com/documentation/web-api/tutorials/code-flow)
   to authorize an account, registering the redirect URI in the dashboard and
   requesting `playlist-read-private` and `playlist-read-collaborative`.
   `scripts/spotify-get-refresh-token` walks through this.
4. Run `/play url:https://open.spotify.com/playlist/PLAYLIST_ID`.
   Add `shuffle:true` to shuffle before enqueueing.

### The 100-track ceiling

Spotify requires user authentication for `GET /playlists/{id}/items`, which is
the only endpoint that paginates. With client credentials the bot falls back to
the tracks embedded in `GET /playlists/{id}`, which returns the first 100 and
ignores `offset`. Imports past that point report the playlist as truncated.

With a refresh token the bot pages properly and imports up to 1000 tracks,
including private and collaborative playlists belonging to the authorized
account. A rotated refresh token is retained in memory; reauthorize and update
`.env` if authorization fails after a restart.

### Resolved-track cache

Searching is the slow part of a playlist import, and the answer never changes,
so each Spotify track ID and the YouTube upload it resolved to are stored in a
[redb](https://github.com/cberner/redb) database at `CACHE_PATH`
(`cache.redb` by default; set it empty to disable). Repeat imports skip the
search entirely. If a cached upload has since been taken down, the bot searches
again. The cache is only an optimisation: if it cannot be opened or read, the
bot logs a warning and behaves as it did without one.

The bot reads playlist pages in order, up to 1000 entries, and searches for each
track's **artist(s) + song name** when playback approaches. It takes the top
YouTube Music song result, and if that does not run within 10 s or 7% of the
Spotify duration, the top regular YouTube result. If neither matches, the track
is skipped. Only the duration is checked, so a cover or live version of the
same length can still be picked.
Local files, episodes, removed tracks and entries without artist/title metadata
are skipped. Failed YouTube playback advances to the next queued track.
Full playlist URLs (including localized `intl-...` URLs and share parameters)
are supported; shortened `spotify.link` URLs, albums, and individual Spotify
track links are not.

A playlist that is neither public nor owned by the authorized account cannot be
read, and Spotify reports it as missing rather than forbidden, so the bot asks
the user to check that the playlist is public. Development
apps also require an app owner with Premium and have user allowlist limits; see
[Spotify's development-mode rules](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide).
All Discord users of this bot use the same configured Spotify account.
Access tokens refresh automatically for imports. If Spotify returns a rotated
refresh token it is retained in memory; reauthorize/update `.env` if authorization
fails after a restart.

## In-memory resolve cache

The redb cache above removes the *search*, but each track still costs a yt-dlp
run to turn its page URL into a direct media URL. Those runs are shared:
`RESOLVE_CACHE_ENTRIES` resolves (512 by default) are kept in memory across
every guild, so the same track queued in two servers, or replayed a few minutes
later, costs one yt-dlp invocation rather than two. Callers that ask for an
input while a run for it is already in flight wait for that run instead of
starting a second one.

This cache has no off switch, and values below 32 are raised to it. Playing a
single link resolves it twice over otherwise: expanding the link already
produces everything playback needs, and the cache is how that reaches the
player. It is the only place a resolve is held — tracks themselves keep none.

An entry lives only as long as the media URL inside it. googlevideo signs its
URLs with an expiry, which the bot reads and honours (minus a two-minute margin,
since playback still has to start on the URL after it is handed out); anything
without one falls back to `RESOLVE_CACHE_TTL_MS`, 10 minutes by default. Signed
URLs are tied to the IP that requested them, and the bot is a single process
behind a single address, so sharing them between guilds is safe. Failed
resolves are never stored.

## Notes on behaviour

**Disconnects drop everything.** Each guild owns one `CancellationToken`; every
yt-dlp child, fetch task and buffer holds a descendant of it. Any voice
disconnect — dropped connection, kick, channel move, shutdown — cancels that one
token and the entire pipeline for that guild goes away.

## Diagnostics

The default `RUST_LOG=slap=info,warn` shows player interactions
(command, button/slash source, user, guild, outcome and elapsed time), playlist
imports, YouTube searches, audio resolution/buffering, actual playback starts,
pauses, track endings, failures and disconnects. Searching/preloading is logged
separately from **now playing**, which is emitted when a track is playable and
playing. Skip and Stop remain silent in Discord but appear in these logs.

Console output uses a dedicated writer with a 1,024-line bounded queue. If the
log destination cannot keep up, excess lines are dropped instead of blocking
playback or growing memory indefinitely. Progress-bar refreshes do not emit
routine logs. Media logs use titles, search terms and source hosts rather than
signed stream URLs, request headers or credentials.

Run with `RUST_LOG=slap=debug` to see buffer depth and stream lifecycle.
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
