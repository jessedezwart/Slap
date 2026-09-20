//! Cache for resolved Spotify tracks. Maps Spotify track IDs to YouTube URLs.
//! Disk-persistent

use std::path::Path;

use redb::{Database, ReadableDatabase, TableDefinition};
use tracing::{debug, warn};

const RESOLVED: TableDefinition<&str, &str> = TableDefinition::new("spotify_to_youtube");

pub struct SpotifyTrackCache {
    db: Database,
}

impl SpotifyTrackCache {
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        // Create table if it doesn't exist yet.
        let write = db.begin_write()?;
        write.open_table(RESOLVED)?;
        write.commit()?;
        Ok(Self { db })
    }

    pub fn get(&self, spotify_id: &str) -> Option<String> {
        let read = || {
            let table = self.db.begin_read()?.open_table(RESOLVED)?;
            Ok::<_, redb::Error>(table.get(spotify_id)?.map(|url| url.value().to_owned()))
        };
        match read() {
            Ok(url) => url,
            Err(error) => {
                warn!(%error, "track cache read failed");
                None
            }
        }
    }

    pub fn put(&self, spotify_id: &str, url: &str) {
        let write = || {
            let txn = self.db.begin_write()?;
            {
                let mut table = txn.open_table(RESOLVED)?;
                table.insert(spotify_id, url)?;
            }
            txn.commit()?;
            Ok::<_, redb::Error>(())
        };
        match write() {
            Ok(()) => debug!(spotify_id, url, "cached resolved track"),
            Err(error) => warn!(%error, "track cache write failed"),
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/audio/cache/spotify_track.rs"]
mod tests;
