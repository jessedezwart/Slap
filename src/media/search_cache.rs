//! Disk persistent cache for resolved searches
use std::{path::Path, sync::Arc};

use redb::{Database, ReadableDatabase, TableDefinition};
use tracing::{debug, info, warn};

const SEARCH_RESULTS: TableDefinition<&str, &str> = TableDefinition::new("search_results");

pub struct SearchCache {
    db: Database,
}

impl SearchCache {
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        // Create table if it doesn't exist yet.
        let write = db.begin_write()?;
        write.open_table(SEARCH_RESULTS)?;
        write.commit()?;
        Ok(Self { db })
    }

    /// Open the cache at `path`, or carry on without one when that fails.
    pub fn open_or_skip(path: &Path) -> Option<Arc<Self>> {
        match Self::open(path) {
            Ok(cache) => {
                info!(path = %path.display(), "search cache opened");
                Some(Arc::new(cache))
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "could not open search cache; continuing without it");
                None
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<String> {
        let read = || {
            let table = self.db.begin_read()?.open_table(SEARCH_RESULTS)?;
            Ok::<_, redb::Error>(table.get(key)?.map(|url| url.value().to_owned()))
        };
        match read() {
            Ok(url) => url,
            Err(error) => {
                warn!(%error, "search cache read failed");
                None
            }
        }
    }

    pub fn put(&self, key: &str, url: &str) {
        let write = || {
            let txn = self.db.begin_write()?;
            {
                let mut table = txn.open_table(SEARCH_RESULTS)?;
                table.insert(key, url)?;
            }
            txn.commit()?;
            Ok::<_, redb::Error>(())
        };
        match write() {
            Ok(()) => debug!(key, url, "cached search result"),
            Err(error) => warn!(%error, "search cache write failed"),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/media/search_cache.rs"]
mod tests;
