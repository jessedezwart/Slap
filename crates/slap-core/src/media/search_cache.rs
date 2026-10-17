//! Remembers which upload a search resolved to, keyed by the
//! [`crate::media::SearchQuery::key`] it was searched for.
//! Disk-persistent

use std::path::Path;

use redb::{Database, ReadableDatabase, TableDefinition};
use tracing::{debug, warn};

/// Search key → URL of the upload it resolved to.
const RESOLVED: TableDefinition<&str, &str> = TableDefinition::new("search_results");

pub struct SearchResultCache {
    db: Database,
}

impl SearchResultCache {
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        // Create table if it doesn't exist yet.
        let write = db.begin_write()?;
        write.open_table(RESOLVED)?;
        write.commit()?;
        Ok(Self { db })
    }

    pub fn get(&self, key: &str) -> Option<String> {
        let read = || {
            let table = self.db.begin_read()?.open_table(RESOLVED)?;
            Ok::<_, redb::Error>(table.get(key)?.map(|url| url.value().to_owned()))
        };
        match read() {
            Ok(url) => url,
            Err(error) => {
                warn!(%error, "track cache read failed");
                None
            }
        }
    }

    pub fn put(&self, key: &str, url: &str) {
        let write = || {
            let txn = self.db.begin_write()?;
            {
                let mut table = txn.open_table(RESOLVED)?;
                table.insert(key, url)?;
            }
            txn.commit()?;
            Ok::<_, redb::Error>(())
        };
        match write() {
            Ok(()) => debug!(key, url, "cached resolved track"),
            Err(error) => warn!(%error, "track cache write failed"),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/media/search_cache.rs"]
mod tests;
