use redb::{DatabaseError, StorageError, TableDefinition, TableError, TransactionError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use tokio::sync::oneshot;

pub const TORRENTS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("torrents");
pub const TRACKERS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("trackers");

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TorrentRecord {
    pub magnet_or_url: String,
    pub started: bool,
    #[serde(default = "default_added_at")]
    pub added_at: i64,
}

pub fn default_added_at() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CachedTrackers {
    pub list: Vec<String>,
    pub updated_at: u64,
}

#[derive(Default, Debug)]
pub struct EngineState {
    pub config: cloud_torrent_common::Config,
    pub torrent_info: HashMap<String, TorrentInfo>,
    pub pending_magnets: HashMap<String, PendingMagnet>,
}

#[derive(Clone, Debug)]
pub struct TorrentInfo {
    pub started: bool,
    pub added_at: i64,
    pub magnet: String,
}

#[derive(Debug)]
pub struct PendingMagnet {
    pub magnet_url: String,
    pub added_at: i64,
    pub cancel_tx: Option<oneshot::Sender<()>>,
}

#[derive(Error, Debug)]
pub enum EngineError {
    #[error("Database error: {0}")]
    Database(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Torrent error: {0}")]
    Torrent(String),
    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<DatabaseError> for EngineError {
    fn from(e: DatabaseError) -> Self {
        Self::Database(e.to_string())
    }
}

impl From<TransactionError> for EngineError {
    fn from(e: TransactionError) -> Self {
        Self::Database(e.to_string())
    }
}

impl From<TableError> for EngineError {
    fn from(e: TableError) -> Self {
        Self::Database(e.to_string())
    }
}

impl From<StorageError> for EngineError {
    fn from(e: StorageError) -> Self {
        Self::Database(e.to_string())
    }
}

pub type EngineResult<T> = Result<T, EngineError>;
