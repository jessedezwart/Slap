# Slap

A Discord music bot that plays audio **without transcode** using yt-dlp and songbird. Designed to be highly efficient.

## Why not use Songbird's built-in YouTube source?

This is an educational exercise. Goal is managing data efficiently across different services without blocking threads, handling disconnects across the pipeline, and seeing how performant I could make it.

There are a few additional advantages:

- Controlling yt-dlp means we can set to use Opus-in-WebM, which is the only format that can be passed through to Discord without transcoding.
- Songbird loads the entire track into memory. This has disadavantages for ram usage and startup time. And since the complete track needs to be loaded on an OS-thread, it can block the complete thread.
- Songbird doesn't cache yt-dlp metadata, which means additional stalling of an OS-thread.
- Since only playback is delegated, I have more control over the failure-handling. I think Songbird orphans instead of kills a yt-dlp process when yt-dlp fails for example.

## Sources

Some supported sources:

- YouTube
- SoundCloud
- Bandcamp
- Spotify playlists (metadata imported from Spotify, audio matched on YouTube; setup below)

Full list is here: https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md

## Docker

Set the token in .env and run:

```sh
docker compose up
```

## Development

Install Rust and yt-dlp, copy `.env.example` to `.env`, set `DISCORD_TOKEN`,
then:

```sh
cargo run
cargo test
```

## Configuration

All configuration settings are documented in `.env.example`. Copy that file to `.env`.

## YouTube DRM

### PO Tokens

YouTube may stop serving audio unless a PO token is sent. The image ships the
[bgutil PO token plugin](https://github.com/Brainicism/bgutil-ytdlp-pot-provider)
and Deno.

### Cookies

If YouTube asks to sign in to confirm you are not a bot, export YouTube cookies
in Netscape/Mozilla cookie-file format to `secrets/cookies.txt`. Follow yt-dlp's
[YouTube cookie export instructions](https://github.com/yt-dlp/yt-dlp/wiki/Extractors#exporting-youtube-cookies).

## Spotify playlists

To use Spotify playlists, you need to create a Spotify app and authorize an account.
Configure `SPOTIFY_CLIENT_ID` and `SPOTIFY_CLIENT_SECRET` in `.env`, then restart the bot.

1. Create an app in the [Spotify developer dashboard](https://developer.spotify.com/dashboard).
2. Put the client ID and secret in `.env`. This is enough to import **public**
   playlists, up to their first 100 tracks.
3. To use public playlists and increase the playlist limit, set `SPOTIFY_REFRESH_TOKEN`. Use
   Spotify's [Authorization Code flow](https://developer.spotify.com/documentation/web-api/tutorials/code-flow)
   to authorize an account, registering the redirect URI in the dashboard and
   requesting `playlist-read-private` and `playlist-read-collaborative`.
   `scripts/spotify-get-refresh-token` walks through this.

## Invite

[Invite the bot](https://discord.com/oauth2/authorize?client_id=881107137990918166&scope=bot%20applications.commands&permissions=3148800)

Replace `client_id` with your bot's client ID. The permissions integer is the minimum needed to join a voice channel and respond to slash commands:

- connect
- speak
- view channel
- send messages (for playback failure warnings)

The invite also includes the `applications.commands` scope for slash commands.
