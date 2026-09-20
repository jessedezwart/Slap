//! The two caches that keep yt-dlp from being run more than it has to be.
//!
//! [`SpotifyTrackCache`] is on disk and remembers, across restarts, which
//! upload a Spotify track was found at. [`YtdlpResolveCache`] is in memory and
//! holds the resolve itself, shared by every guild.

pub mod spotify_track;
pub mod ytdlp_resolve;

pub use spotify_track::SpotifyTrackCache;
pub use ytdlp_resolve::YtdlpResolveCache;
