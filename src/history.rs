//! All-time play stats per guild, kept on disk.
use std::{path::Path, sync::Arc, time::Duration};

use redb::{Database, ReadableDatabase, ReadableTable, Table, TableDefinition};
use tracing::{debug, info, warn};
use twilight_model::id::{
    Id,
    marker::{GuildMarker, UserMarker},
};
use url::Url;

use crate::session::{PlayStats, Played};

/// Guild to (tracks, milliseconds played, sessions).
const TOTALS: TableDefinition<u64, (u64, u64, u64)> = TableDefinition::new("totals");
/// (guild, link or title) to (count, label, link).
const TRACKS: TableDefinition<(u64, &str), (u64, &str, Option<&str>)> =
    TableDefinition::new("tracks");
/// (guild, link or name) to (count, label, link).
const PLAYLISTS: TableDefinition<(u64, &str), (u64, &str, Option<&str>)> =
    TableDefinition::new("playlists");
/// (guild, user) to sessions they started.
const STARTERS: TableDefinition<(u64, u64), u64> = TableDefinition::new("starters");

/// What a guild has played, over every session.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GuildSummary {
    pub tracks: u64,
    pub duration: Duration,
    pub sessions: u64,
    /// Most played first.
    pub top_tracks: Vec<Ranked>,
    /// Most imported first.
    pub top_playlists: Vec<Ranked>,
    /// Most sessions started first.
    pub top_starters: Vec<(Id<UserMarker>, u64)>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Ranked {
    pub label: String,
    pub url: Option<Url>,
    pub count: u64,
}

pub struct PlayHistory {
    db: Database,
}

impl PlayHistory {
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        // Create tables if they don't exist yet, so reads never miss one.
        let write = db.begin_write()?;
        write.open_table(TOTALS)?;
        write.open_table(TRACKS)?;
        write.open_table(PLAYLISTS)?;
        write.open_table(STARTERS)?;
        write.commit()?;
        Ok(Self { db })
    }

    /// Open the history at `path`, or carry on without one when that fails.
    pub fn open_or_skip(path: &Path) -> Option<Arc<Self>> {
        match Self::open(path) {
            Ok(history) => {
                info!(path = %path.display(), "play history opened");
                Some(Arc::new(history))
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "could not open play history; continuing without it");
                None
            }
        }
    }

    /// Add an ended session's `stats` to the guild's totals.
    pub fn record(&self, guild_id: Id<GuildMarker>, stats: &PlayStats) {
        if stats.started_with.is_none() && stats.tracks == 0 {
            return;
        }
        match self.write(guild_id.get(), stats) {
            Ok(()) => debug!(%guild_id, tracks = stats.tracks, "saved play history"),
            Err(error) => warn!(%guild_id, %error, "play history write failed"),
        }
    }

    fn write(&self, guild: u64, stats: &PlayStats) -> Result<(), redb::Error> {
        let txn = self.db.begin_write()?;
        {
            let mut totals = txn.open_table(TOTALS)?;
            let (tracks, millis, sessions) = totals.get(guild)?.map_or((0, 0, 0), |v| v.value());
            let added = u64::try_from(stats.duration.as_millis()).unwrap_or(u64::MAX);
            totals.insert(
                guild,
                (
                    tracks + stats.tracks as u64,
                    millis.saturating_add(added),
                    sessions + u64::from(stats.started_with.is_some()),
                ),
            )?;

            let mut tracks = txn.open_table(TRACKS)?;
            for played in &stats.played {
                bump(&mut tracks, guild, played)?;
            }
            let mut playlists = txn.open_table(PLAYLISTS)?;
            for played in &stats.playlists {
                bump(&mut playlists, guild, played)?;
            }

            if let Some(started) = &stats.started_with {
                let mut starters = txn.open_table(STARTERS)?;
                let key = (guild, started.requester.get());
                let count = starters.get(key)?.map_or(0, |v| v.value());
                starters.insert(key, count + 1)?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// The guild's totals, with at most `top` entries in each ranking.
    pub fn summary(&self, guild_id: Id<GuildMarker>, top: usize) -> Option<GuildSummary> {
        match self.read(guild_id.get(), top) {
            Ok(summary) => Some(summary),
            Err(error) => {
                warn!(%guild_id, %error, "play history read failed");
                None
            }
        }
    }

    fn read(&self, guild: u64, top: usize) -> Result<GuildSummary, redb::Error> {
        let txn = self.db.begin_read()?;
        let (tracks, millis, sessions) = txn
            .open_table(TOTALS)?
            .get(guild)?
            .map_or((0, 0, 0), |v| v.value());

        let mut top_starters = Vec::new();
        for entry in txn
            .open_table(STARTERS)?
            .range((guild, 0)..=(guild, u64::MAX))?
        {
            let (key, count) = entry?;
            if let Some(user) = Id::new_checked(key.value().1) {
                top_starters.push((user, count.value()));
            }
        }
        top_starters.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        top_starters.truncate(top);

        Ok(GuildSummary {
            tracks,
            duration: Duration::from_millis(millis),
            sessions,
            top_tracks: ranked(&txn.open_table(TRACKS)?, guild, top)?,
            top_playlists: ranked(&txn.open_table(PLAYLISTS)?, guild, top)?,
            top_starters,
        })
    }
}

/// Count one more play of `played`, keyed by its link, else its label.
fn bump(
    table: &mut Table<(u64, &str), (u64, &str, Option<&str>)>,
    guild: u64,
    played: &Played,
) -> Result<(), redb::Error> {
    let url = played.url.as_ref().map(Url::as_str);
    let key = (guild, url.unwrap_or(&played.label));
    let count = table.get(key)?.map_or(0, |v| v.value().0);
    // Keep the latest label, as titles and playlist names change.
    table.insert(key, (count + 1, played.label.as_str(), url))?;
    Ok(())
}

/// The guild's `top` entries of `table`, highest count first.
fn ranked(
    table: &impl ReadableTable<(u64, &'static str), (u64, &'static str, Option<&'static str>)>,
    guild: u64,
    top: usize,
) -> Result<Vec<Ranked>, redb::Error> {
    let mut ranked = Vec::new();
    for entry in table.range((guild, "")..)? {
        let (key, value) = entry?;
        if key.value().0 != guild {
            break;
        }
        let (count, label, url) = value.value();
        ranked.push(Ranked {
            label: label.to_owned(),
            url: url.and_then(|url| url.parse().ok()),
            count,
        });
    }
    ranked.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
    ranked.truncate(top);
    Ok(ranked)
}

#[cfg(test)]
#[path = "../tests/history/store.rs"]
mod tests;
