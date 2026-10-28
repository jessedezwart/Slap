# Slap

[![Add to Discord](https://img.shields.io/badge/Add%20to-Discord-5865F2?logo=discord&logoColor=white)](https://discord.com/oauth2/authorize?client_id=881107137990918166&scope=bot%20applications.commands&permissions=3148800)

Highly performant Discord music bot that plays music **without transcode** using yt-dlp and songbird. Can be ran from a cheap VPS.

## Architecture

### Key Libraries

| Library | Role |
| --- | --- |
| [twilight](https://github.com/twilight-rs/twilight) | Discord gateway, REST and slash commands. |
| [songbird](https://github.com/serenity-rs/songbird) | Songbird provides a complete music playback solution, only the driver and gateway parts are used. |
| [symphonia](https://github.com/pdeljanov/Symphonia) | MKV/WebM and Ogg demuxing, so Opus packets go to Discord without being decoded. |
| [yt-dlp](https://github.com/yt-dlp/yt-dlp) (external process) | Resolve media URLs. |

### Why not use Songbird's built-in YouTube source?

This is an educational exercise. Goal is to async manage IO in Rust across different services (ytdlp -> buffer media -> Discord). It's a good exercise to learn about the Rust ownership model.

There are a few additional advantages:

- Controlling yt-dlp means we can set it to only use Opus-in-WebM, which is the only format that can be passed through to Discord without transcoding.
- Songbird loads the entire track into memory. This massively increases RAM usage. And since the complete track needs to be loaded on an OS-thread, it can block the complete thread.
- More control over the failure-handling. For example: Songbird orphans a yt-dlp process when it fails, while it should be killed instead.

## Sources

### YouTube

Some supported sources:

- YouTube
- SoundCloud

Full list is here: https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md

Only when a source supports Opus will it be used.

### Spotify

Spotify is supported when the Spotify API credentials are set. The name and artist of a Spotify track is looked up on YouTube Music.

## Deployment

### Docker (recommended)

Set the token in .env and optionally secrets and run:

```sh
docker compose up
```

### Prebuilt binaries

- Download from [release](https://github.com/jessedezwart/Slap/releases). On Windows, use `slap-windows-x86_64-setup-full.exe` to also get PO tokens (see below), or `slap-windows-x86_64-setup.exe` for a smaller install with only yt-dlp.
- In the installation folder (`%LOCALAPPDATA%\Programs\Slap` by default), change the .env and optionally add cookies (see below).

### Build locally (Windows)

Get dependencies:

```powershell
# Get Rust
winget install --id Rustlang.Rustup -e
# Get C++ compiler
winget install --id Microsoft.VisualStudio.BuildTools -e --override "--passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install --id yt-dlp.yt-dlp -e
```

Restart the terminal, then:

```powershell
# Clone this repo
git clone https://github.com/jessedezwart/Slap
# Run
cargo run --release
```


## Configuration

All configuration settings are documented in `.env.example`. Copy that file to `.env`.

### ytdlp

ytdlp will work by default. But after some use, access might get restricted. To prevent this, cookies and PO tokens can help.

#### PO Tokens

YouTube may stop serving audio unless a PO token is sent. The  Docker Compose ships the [bgutil PO token plugin](https://github.com/Brainicism/bgutil-ytdlp-pot-provider) and Deno. So does the full Windows installer (`slap-windows-x86_64-setup-full.exe`).

More info can be found in the [ytdlp PO Token Guide](https://github.com/yt-dlp/yt-dlp/wiki/Po-Token-Guide).

#### Cookies

If YouTube asks to sign in to confirm you are not a bot, export YouTube cookies in Netscape/Mozilla cookie-file format to `secrets/cookies.txt`. Follow yt-dlp's [YouTube cookie export instructions](https://github.com/yt-dlp/yt-dlp/wiki/Extractors#exporting-youtube-cookies).

### Spotify API

Since Februari 2026, only your own playlists can be found using the Spotify API.

To use the playlists, you need to create a Spotify app and authorize an account.
Configure `SPOTIFY_CLIENT_ID` and `SPOTIFY_CLIENT_SECRET` in `.env`, then restart the bot.

1. Create an app in the [Spotify developer dashboard](https://developer.spotify.com/dashboard).
2. Put the client ID and secret in `.env`. This is enough to import **public** playlists, up to their first 100 tracks.
3. To use public playlists and increase the playlist limit, set `SPOTIFY_REFRESH_TOKEN`. Use Spotify's [Authorization Code flow](https://developer.spotify.com/documentation/web-api/tutorials/code-flow) to authorize an account, registering the redirect URI in the dashboard and requesting `playlist-read-private` and `playlist-read-collaborative`. `scripts/spotify-get-refresh-token` walks through this.

### Discord

#### Get the bot token

1. Go to [Discord Developer Portal]<https://discord.com/developers/home>
2. Create an application
3. Go to **Overview** > **Bot**
4. Get the token (might need to click **Reset Token**)
5. Set in .env

#### Create invite link

Replace the Client ID in this url with the application's client ID (can be found in **Overview** > **General Information**).
`https://discord.com/oauth2/authorize?client_id={client_id}&scope=bot%20applications.commands&permissions=3148800)`

##### Permissions

The permissions as set above resolve to:

- connect, for joining voice channel.
- speak, needed for playing the voice
- view channel
- send messages, 

The invite also includes the `applications.commands` scope for slash commands.
