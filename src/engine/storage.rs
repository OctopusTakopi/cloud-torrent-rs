use crate::engine::types::{
    CachedTrackers, EngineResult, PersistedRssState, RSS_TABLE, TORRENTS_TABLE, TRACKERS_TABLE,
    TorrentRecord,
};
use redb::{Database, ReadableDatabase, ReadableTable};
use std::sync::Arc;

pub struct Storage {
    db: Arc<Database>,
}

impl Storage {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn save_torrent(&self, info_hash: &str, record: &TorrentRecord) -> EngineResult<()> {
        let json = serde_json::to_string(record)?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TORRENTS_TABLE)
                .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
            table
                .insert(info_hash, json.as_str())
                .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn remove_torrent(&self, info_hash: &str) -> EngineResult<()> {
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TORRENTS_TABLE)
                .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
            table
                .remove(info_hash)
                .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn load_torrents(&self) -> EngineResult<Vec<(String, TorrentRecord)>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TORRENTS_TABLE)
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        let mut results = Vec::new();
        for result in table
            .iter()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?
        {
            let (key, value) =
                result.map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
            let record: TorrentRecord = serde_json::from_str(value.value())?;
            results.push((key.value().to_string(), record));
        }
        Ok(results)
    }

    pub fn save_trackers(&self, url: &str, trackers: &CachedTrackers) -> EngineResult<()> {
        let json = serde_json::to_string(trackers)?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TRACKERS_TABLE)
                .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
            table
                .insert(url, json.as_str())
                .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn load_trackers(&self, url: &str) -> EngineResult<Option<CachedTrackers>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TRACKERS_TABLE)
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        if let Some(value) = table
            .get(url)
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?
        {
            let cached: CachedTrackers = serde_json::from_str(value.value())?;
            Ok(Some(cached))
        } else {
            Ok(None)
        }
    }

    pub fn save_rss_state(&self, rss_state: &PersistedRssState) -> EngineResult<()> {
        let json = serde_json::to_string(rss_state)?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(RSS_TABLE)
                .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
            table
                .insert("state", json.as_str())
                .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn load_rss_state(&self) -> EngineResult<Option<PersistedRssState>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(RSS_TABLE)
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?;
        if let Some(value) = table
            .get("state")
            .map_err(|e| crate::engine::types::EngineError::Database(e.to_string()))?
        {
            let cached: PersistedRssState = serde_json::from_str(value.value())?;
            Ok(Some(cached))
        } else {
            Ok(None)
        }
    }
}
