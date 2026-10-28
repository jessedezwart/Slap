//! Text helpers shared by command replies and the player.

use std::time::Duration;

use url::Url;

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

/// Keep `text` on one line inside a code block, at most `max_chars` long when given.
pub(super) fn code_line(text: &str, max_chars: Option<usize>) -> String {
    text.chars()
        .take(max_chars.unwrap_or(usize::MAX))
        // Replace newlines and backticks with safe characters
        .map(|c| match c {
            '\n' | '\r' => ' ',
            '`' => '\'',
            _ => c,
        })
        .collect()
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

/// Bold, escaped `label`, linked when `url` is safe to embed: HTTPS only, and
/// no `>`, which would end the suppressed-embed `<…>` early.
pub(super) fn link(label: &str, url: Option<&Url>) -> String {
    let label = escape(label);
    match url
        .map(Url::as_str)
        .filter(|url| url.starts_with("https://") && !url.contains('>'))
    {
        Some(url) => format!("**[{label}](<{url}>)**"),
        None => format!("**{label}**"),
    }
}

#[cfg(test)]
#[path = "../../tests/discord/text.rs"]
mod tests;
