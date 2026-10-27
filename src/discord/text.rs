//! Text helpers shared by command replies and the player.

use std::time::Duration;

use url::Url;

use crate::voice::session::Playlist;

/// Stop a track title from breaking out of its bold markers or pinging anyone.
pub(super) fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars().take(200) {
        match c {
            '\\' | '*' | '_' | '`' | '~' | '|' | '[' | ']' => {
                escaped.push('\\');
                escaped.push(c);
            }
            '\n' | '\r' => escaped.push(' '),
            '@' => escaped.push_str("@\u{200b}"),
            _ => escaped.push(c),
        }
    }
    escaped
}

pub(super) fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Bold `label`, linked when `url` is safe to embed: HTTPS only, and no `>`,
/// which would end the suppressed-embed `<…>` early.
pub(super) fn link(label: &str, url: Option<&Url>) -> String {
    match url
        .map(Url::as_str)
        .filter(|url| url.starts_with("https://") && !url.contains('>'))
    {
        Some(url) => format!("**[{label}](<{url}>)**"),
        None => format!("**{label}**"),
    }
}

/// A playlist's name, linked to the playlist it was imported from.
pub(super) fn playlist_link(playlist: &Playlist) -> String {
    link(&escape(&playlist.name), Some(&playlist.url))
}

#[cfg(test)]
#[path = "../../tests/discord/text.rs"]
mod tests;
