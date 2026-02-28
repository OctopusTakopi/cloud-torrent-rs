use anyhow::{Context, Result};
pub use cloud_torrent_common::{Config, Torrent};
use librqbit::api::TorrentIdOrHash;
use librqbit::{AddTorrent, AddTorrentOptions, Session, SessionOptions, TorrentStatsState};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};

const TORRENTS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("torrents");
const TRACKERS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("trackers");
const CACHE_PREFIX: &str = "_CLDAUTOSAVED_";
const TRACKER_CACHE_TTL: u64 = 3600 * 24; // 24 hours

#[derive(Serialize, Deserialize)]
struct TorrentRecord {
    pub magnet_or_url: String,
    pub started: bool,
    #[serde(default = "default_added_at")]
    pub added_at: i64,
}

fn default_added_at() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[derive(Serialize, Deserialize)]
struct CachedTrackers {
    pub list: Vec<String>,
    pub updated_at: u64,
}

pub struct EngineState {
    pub config: Config,
    pub torrent_started: HashMap<String, bool>,
    pub torrent_added_at: HashMap<String, i64>,
    pub torrent_magnets: HashMap<String, String>,
    pub pending_magnets: HashMap<String, (String, i64, Option<tokio::sync::oneshot::Sender<()>>)>, // hash -> (magnet_url, added_at, tx)
}

#[derive(Clone)]
pub struct Engine {
    pub state: Arc<RwLock<EngineState>>,
    pub changed_tx: mpsc::Sender<()>,
    pub session: Arc<Session>,
    pub db: Arc<Database>,
}

/// Build a consistent HTTP client that prefers IPv4 over IPv6.
/// This prevents failures on machines where IPv6 is available in DNS but broken at the network level.
pub fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36")
        .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
        .build()
        .unwrap_or_default()
}

fn parse_rate(rate: &str) -> Option<u32> {
    if rate.is_empty() || rate == "0" {
        return None;
    }
    let mut rate = rate.to_lowercase();
    let suffixes = ["/s", "b/s", "b", "ps"];
    for s in suffixes {
        if let Some(stripped) = rate.strip_suffix(s) {
            rate = stripped.to_string();
        }
    }
    rate = rate.trim().to_string();

    let multiplier = if rate.ends_with('k') {
        rate.pop();
        1024
    } else if rate.ends_with('m') {
        rate.pop();
        1024 * 1024
    } else if rate.ends_with('g') {
        rate.pop();
        1024 * 1024 * 1024
    } else {
        1
    };

    rate.trim()
        .parse::<f64>()
        .ok()
        .map(|n| (n * multiplier as f64) as u32)
}

impl Engine {
    pub async fn new(config: Config) -> Result<(Self, mpsc::Receiver<()>)> {
        let download_path = Path::new(&config.download_directory);
        if !download_path.exists() {
            std::fs::create_dir_all(download_path)?;
        }

        let cache_path = Path::new(&config.cache_directory);
        if !cache_path.exists() {
            std::fs::create_dir_all(cache_path)?;
        }

        let trash_path = Path::new(&config.trash_directory);
        if !trash_path.exists() {
            std::fs::create_dir_all(trash_path)?;
        }

        // Initialize redb
        let db_path = cache_path.join("cloud-torrent.db");
        let db = Database::create(db_path)?;

        // Ensure tables exist
        {
            let write_txn = db.begin_write()?;
            {
                let _ = write_txn.open_table(TORRENTS_TABLE)?;
                let _ = write_txn.open_table(TRACKERS_TABLE)?;
            }
            write_txn.commit()?;
        }

        let mut options = SessionOptions {
            disable_dht: config.disable_trackers,
            ..Default::default()
        };

        if let Some(down) = parse_rate(&config.download_rate) {
            options.ratelimits.download_bps = NonZeroU32::new(down);
        }
        if config.enable_upload {
            if let Some(up) = parse_rate(&config.upload_rate) {
                options.ratelimits.upload_bps = NonZeroU32::new(up);
            }
        } else {
            options.ratelimits.upload_bps = NonZeroU32::new(1);
        }

        let session = Session::new_with_opts(download_path.to_path_buf(), options).await?;

        let (tx, rx) = mpsc::channel(100);
        let engine = Self {
            state: Arc::new(RwLock::new(EngineState {
                config,
                torrent_started: HashMap::new(),
                torrent_added_at: HashMap::new(),
                torrent_magnets: HashMap::new(),
                pending_magnets: HashMap::new(),
            })),
            changed_tx: tx,
            session: session.clone(),
            db: Arc::new(db),
        };

        // Background manager loop
        let engine_clone = engine.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                let config = engine_clone.get_config().await;
                let torrents = engine_clone.session.with_torrents(|torrents| {
                    torrents.map(|(id, h)| (id, h.clone())).collect::<Vec<_>>()
                });

                let mut downloading_count: i32 = 0;
                let mut active_count: i32 = 0;
                let mut queue = vec![];

                for (id, h) in torrents {
                    let stats = h.stats();
                    let info_hash = h.info_hash().as_string();
                    let is_started = engine_clone
                        .state
                        .read()
                        .await
                        .torrent_started
                        .get(&info_hash)
                        .cloned()
                        .unwrap_or(true);

                    // Enable Seeding check
                    if !config.enable_seeding
                        && stats.finished
                        && !matches!(stats.state, TorrentStatsState::Paused)
                    {
                        tracing::info!(
                            "Torrent {} finished and seeding is disabled, stopping.",
                            id
                        );
                        let _ = engine_clone.session.pause(&h).await;
                        continue;
                    }

                    // Seed Ratio check
                    if config.seed_ratio > 0.0
                        && stats.finished
                        && !matches!(stats.state, TorrentStatsState::Paused)
                    {
                        let ratio = if stats.progress_bytes > 0 {
                            stats.uploaded_bytes as f32 / stats.progress_bytes as f32
                        } else {
                            0.0
                        };
                        if ratio >= config.seed_ratio {
                            tracing::info!(
                                "Torrent {} reached seed ratio {}, stopping.",
                                id,
                                config.seed_ratio
                            );
                            let _ = engine_clone.session.pause(&h).await;
                            continue;
                        }
                    }

                    if !matches!(stats.state, TorrentStatsState::Paused) {
                        active_count += 1;
                        if !stats.finished {
                            downloading_count += 1;
                        }

                        // Max Active check
                        if config.max_active_torrents > 0
                            && active_count > config.max_active_torrents
                        {
                            tracing::info!(
                                "Max active torrents reached ({}), pausing torrent {}.",
                                config.max_active_torrents,
                                id
                            );
                            let _ = engine_clone.session.pause(&h).await;
                            active_count -= 1;
                            if !stats.finished {
                                downloading_count -= 1;
                            }
                            continue;
                        }

                        // Max Concurrent Task check
                        if !stats.finished
                            && config.max_concurrent_task > 0
                            && downloading_count > config.max_concurrent_task
                        {
                            tracing::info!(
                                "Max concurrent tasks reached ({}), pausing torrent {}.",
                                config.max_concurrent_task,
                                id
                            );
                            let _ = engine_clone.session.pause(&h).await;
                            downloading_count -= 1;
                            active_count -= 1;
                            continue;
                        }
                    } else if !stats.finished && is_started {
                        queue.push(h);
                    }
                }

                // If we have room, start torrents from the queue
                if (downloading_count < config.max_concurrent_task
                    || config.max_concurrent_task == 0)
                    && (active_count < config.max_active_torrents
                        || config.max_active_torrents == 0)
                {
                    for h in queue {
                        if config.max_concurrent_task > 0
                            && downloading_count >= config.max_concurrent_task
                        {
                            break;
                        }
                        if config.max_active_torrents > 0
                            && active_count >= config.max_active_torrents
                        {
                            break;
                        }
                        tracing::info!("Starting queued torrent {}.", h.id());
                        let _ = engine_clone.session.unpause(&h).await;
                        downloading_count += 1;
                        active_count += 1;
                    }
                }
            }
        });

        // Restore torrents from DB
        engine.restore_torrents().await?;

        Ok((engine, rx))
    }

    async fn restore_torrents(&self) -> Result<()> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TORRENTS_TABLE)?;

        let trackers = self.get_trackers(true).await;
        let restore_semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(8));

        for result in table.iter()? {
            let (key, value) = result?;
            let info_hash = key.value();
            let raw_value = value.value();

            let record = if let Ok(rec) = serde_json::from_str::<TorrentRecord>(raw_value) {
                rec
            } else {
                TorrentRecord {
                    magnet_or_url: raw_value.to_string(),
                    started: true,
                    added_at: default_added_at(),
                }
            };

            tracing::info!("Restoring torrent: {}", info_hash);
            {
                let mut state = self.state.write().await;
                state
                    .torrent_started
                    .insert(info_hash.to_string(), record.started);
                state
                    .torrent_added_at
                    .insert(info_hash.to_string(), record.added_at);
                state
                    .torrent_magnets
                    .insert(info_hash.to_string(), record.magnet_or_url.clone());
            }

            let magnet_or_url = &record.magnet_or_url;
            let auto_start = record.started;
            let opts = AddTorrentOptions {
                paused: !auto_start,
                overwrite: true,
                ..Default::default()
            };

            if let Some(hex_str) = magnet_or_url.strip_prefix("torrent_bytes:") {
                if let Ok(bytes) = hex::decode(hex_str) {
                    let engine_clone = self.clone();
                    let info_hash = info_hash.to_string();
                    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                    {
                        let mut state = self.state.write().await;
                        state.pending_magnets.insert(
                            info_hash.clone(),
                            (record.magnet_or_url.clone(), record.added_at, Some(tx)),
                        );
                    }
                    let restore_semaphore = restore_semaphore.clone();
                    tokio::spawn(async move {
                        let permit = restore_semaphore.acquire_owned().await.unwrap();
                        let mut permit_opt = Some(permit);
                        let sleep_fut = tokio::time::sleep(std::time::Duration::from_secs(60));
                        tokio::pin!(sleep_fut);

                        let add_fut = engine_clone
                            .session
                            .add_torrent(AddTorrent::from_bytes(bytes), Some(opts));
                        tokio::pin!(add_fut);
                        let mut rx = rx;

                        let res = loop {
                            tokio::select! {
                                r = &mut add_fut => break r,
                                _ = &mut sleep_fut, if permit_opt.is_some() => {
                                    tracing::warn!("Restore of bytes {} exceeded 60s. Releasing permit.", info_hash);
                                    permit_opt.take();
                                },
                                _ = &mut rx => {
                                    tracing::info!("Restore {} was cancelled.", info_hash);
                                    return;
                                }
                            }
                        };
                        {
                            let mut state = engine_clone.state.write().await;
                            state.pending_magnets.remove(&info_hash);
                        }
                        if let Err(e) = res {
                            tracing::error!("Failed to restore torrent bytes {}: {}", info_hash, e);
                        }
                    });
                }
                continue;
            }

            let mut final_magnet = magnet_or_url.to_string();
            if magnet_or_url.starts_with("magnet:") {
                let has_trackers = final_magnet.contains("&tr=");
                let config = self.get_config().await;
                if config.always_add_trackers || !has_trackers {
                    for tr in &trackers {
                        let encoded_tr = urlencoding::encode(tr);
                        if !final_magnet.contains(&encoded_tr.to_string()) {
                            final_magnet.push_str("&tr=");
                            final_magnet.push_str(&encoded_tr);
                        }
                    }
                }
            }

            let engine_clone = self.clone();
            let info_hash = info_hash.to_string();
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            {
                let mut state = self.state.write().await;
                state.pending_magnets.insert(
                    info_hash.clone(),
                    (record.magnet_or_url.clone(), record.added_at, Some(tx)),
                );
            }

            let restore_semaphore = restore_semaphore.clone();
            tokio::spawn(async move {
                let permit = restore_semaphore.acquire_owned().await.unwrap();
                let mut permit_opt = Some(permit);
                let sleep_fut = tokio::time::sleep(std::time::Duration::from_secs(60));
                tokio::pin!(sleep_fut);

                let add_fut = engine_clone
                    .session
                    .add_torrent(AddTorrent::from_url(&final_magnet), Some(opts));
                tokio::pin!(add_fut);
                let mut rx = rx;

                let res = loop {
                    tokio::select! {
                        r = &mut add_fut => break r,
                        _ = &mut sleep_fut, if permit_opt.is_some() => {
                            tracing::warn!("Restore of {} exceeded 60s. Releasing permit.", info_hash);
                            permit_opt.take();
                        },
                        _ = &mut rx => {
                            tracing::info!("Restore {} was cancelled.", info_hash);
                            return;
                        }
                    }
                };
                {
                    let mut state = engine_clone.state.write().await;
                    state.pending_magnets.remove(&info_hash);
                }
                if let Err(e) = res {
                    tracing::error!("Failed to restore torrent {}: {}", info_hash, e);
                }
            });
        }
        Ok(())
    }

    pub async fn update_config(&self, mut new_config: Config) -> Result<()> {
        let download_path = std::path::Path::new(&new_config.download_directory);
        let cache_dir = download_path.join(".cache");
        let trash_dir = download_path.join(".trash");
        let _ = std::fs::create_dir_all(&cache_dir);
        let _ = std::fs::create_dir_all(&trash_dir);
        new_config.cache_directory = cache_dir.to_string_lossy().into_owned();
        new_config.trash_directory = trash_dir.to_string_lossy().into_owned();
        // Apply rate limits
        let down_bps = parse_rate(&new_config.download_rate).and_then(NonZeroU32::new);
        let up_bps = if new_config.enable_upload {
            parse_rate(&new_config.upload_rate).and_then(NonZeroU32::new)
        } else {
            NonZeroU32::new(1)
        };

        self.session.ratelimits.set_download_bps(down_bps);
        self.session.ratelimits.set_upload_bps(up_bps);

        // Save to file
        let yaml = serde_yaml::to_string(&new_config)?;
        std::fs::write("cloud-torrent.yaml", yaml)?;

        let mut state = self.state.write().await;
        state.config = new_config;
        let _ = self.changed_tx.try_send(());
        Ok(())
    }

    async fn get_cache_paths(&self, info_hash: &str) -> (PathBuf, PathBuf) {
        let state = self.state.read().await;
        let filename = format!("{}{}.info", CACHE_PREFIX, info_hash);
        let cache_file = Path::new(&state.config.cache_directory).join(&filename);
        let trash_file = Path::new(&state.config.trash_directory).join(&filename);
        (cache_file, trash_file)
    }

    pub async fn add_torrent_bytes(&self, bytes: Vec<u8>) -> Result<()> {
        let auto_start = self.state.read().await.config.auto_start;
        let opts = AddTorrentOptions {
            paused: !auto_start,
            overwrite: true,
            ..Default::default()
        };

        let res = self
            .session
            .add_torrent(AddTorrent::from_bytes(bytes.clone()), Some(opts))
            .await?;
        let handle = res.into_handle().context("failed to get torrent handle")?;
        let info_hash = handle.info_hash().as_string();

        // Persist to redb
        {
            let added_at = if auto_start { default_added_at() } else { 0 };
            let record = TorrentRecord {
                magnet_or_url: format!("torrent_bytes:{}", hex::encode(&bytes)),
                started: auto_start,
                added_at,
            };
            let json = serde_json::to_string(&record)?;
            let write_txn = self.db.begin_write()?;
            {
                let mut table = write_txn.open_table(TORRENTS_TABLE)?;
                table.insert(info_hash.as_str(), json.as_str())?;
            }
            write_txn.commit()?;
            let mut state = self.state.write().await;
            state.torrent_started.insert(info_hash.clone(), auto_start);
            state
                .torrent_added_at
                .insert(info_hash.clone(), record.added_at);
            state
                .torrent_magnets
                .insert(info_hash.clone(), record.magnet_or_url.clone());
        }

        // Create .torrent file in cache_dir (Go style)
        let filename = format!("{}{}.torrent", CACHE_PREFIX, info_hash);
        let cache_file = Path::new(&self.state.read().await.config.cache_directory).join(&filename);
        if !cache_file.exists() {
            let _ = std::fs::write(cache_file, bytes);
        }

        let _ = self.changed_tx.try_send(());
        Ok(())
    }

    pub async fn add_magnet(&self, magnet: &str) -> Result<()> {
        tracing::info!("Engine: handling magnet/command: {}", magnet);

        if let Some(ih_hex) = magnet.strip_prefix("start:") {
            let handle = self.session.with_torrents(|torrents| {
                for (_, h) in torrents {
                    if h.info_hash().as_string() == ih_hex {
                        return Some(h.clone());
                    }
                }
                None
            });
            if let Some(h) = handle {
                let _ = self.session.unpause(&h).await;
                let now = default_added_at();
                {
                    let mut state = self.state.write().await;
                    state.torrent_started.insert(ih_hex.to_string(), true);
                    state.torrent_added_at.insert(ih_hex.to_string(), now);
                }
                let _ = self.update_torrent_record(ih_hex, true, Some(now));
            }
            let _ = self.changed_tx.try_send(());
            return Ok(());
        }

        if let Some(ih_hex) = magnet.strip_prefix("stop:") {
            let handle = self.session.with_torrents(|torrents| {
                for (_, h) in torrents {
                    if h.info_hash().as_string() == ih_hex {
                        return Some(h.clone());
                    }
                }
                None
            });
            if let Some(h) = handle {
                let _ = self.session.pause(&h).await;
                {
                    let mut state = self.state.write().await;
                    state.torrent_started.insert(ih_hex.to_string(), false);
                    state.torrent_added_at.insert(ih_hex.to_string(), 0);
                }
                let _ = self.update_torrent_record(ih_hex, false, Some(0));
            }
            let _ = self.changed_tx.try_send(());
            return Ok(());
        }

        if let Some(ih_hex) = magnet.strip_prefix("delete:") {
            let target_id = self.session.with_torrents(|torrents| {
                for (id, h) in torrents {
                    if h.info_hash().as_string() == ih_hex {
                        return Some(id);
                    }
                }
                None
            });
            if let Some(id) = target_id {
                // Delete from DB
                {
                    if let Ok(write_txn) = self.db.begin_write() {
                        if let Ok(mut table) = write_txn.open_table(TORRENTS_TABLE) {
                            let _ = table.remove(ih_hex);
                        }
                        let _ = write_txn.commit();
                    }
                }

                // Move file to trash_dir
                let (cache_file, trash_file) = self.get_cache_paths(ih_hex).await;
                if cache_file.exists() {
                    let _ = std::fs::rename(cache_file, trash_file);
                }

                let _ = self.session.delete(TorrentIdOrHash::Id(id), false).await;
            } else {
                // Not in session, might be pending
                let removed = {
                    let mut state = self.state.write().await;
                    if let Some((_, _, tx)) = state.pending_magnets.remove(ih_hex) {
                        if let Some(tx) = tx {
                            let _ = tx.send(());
                        }
                        true
                    } else {
                        false
                    }
                };
                if removed {
                    tracing::info!("Removed pending magnet {}", ih_hex);
                }
            }
            let _ = self.changed_tx.try_send(());
            return Ok(());
        }

        let config = self.get_config().await;
        let auto_start = config.auto_start;
        let opts = AddTorrentOptions {
            paused: !auto_start,
            overwrite: true,
            ..Default::default()
        };

        let final_magnet = magnet.to_string();

        if final_magnet.starts_with("http://") || final_magnet.starts_with("https://") {
            tracing::info!("Fetching HTTP torrent from {}", final_magnet);
            let client = build_http_client();
            let resp = client.get(&final_magnet).send().await?;
            let bytes = resp.bytes().await?.to_vec();
            return self.add_torrent_bytes(bytes).await;
        }

        let engine_clone = self.clone();
        let magnet_clone = magnet.to_string();

        let info_hash_pending = if magnet_clone.starts_with("magnet:") {
            if let Ok(m) = librqbit::Magnet::parse(&magnet_clone) {
                m.as_id20().map(|h| h.as_string())
            } else {
                None
            }
        } else {
            None
        };

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        if let Some(hash) = &info_hash_pending {
            let mut state = self.state.write().await;
            state.pending_magnets.insert(
                hash.clone(),
                (magnet_clone.clone(), default_added_at(), Some(tx)),
            );
        }

        tokio::spawn(async move {
            let mut final_magnet = magnet_clone.clone();

            if magnet_clone.starts_with("magnet:") {
                let has_trackers = final_magnet.contains("&tr=");
                if config.always_add_trackers || !has_trackers {
                    let trackers = engine_clone.get_trackers(false).await;
                    for tr in trackers {
                        let encoded_tr = urlencoding::encode(&tr);
                        if !final_magnet.contains(&encoded_tr.to_string()) {
                            final_magnet.push_str("&tr=");
                            final_magnet.push_str(&encoded_tr);
                        }
                    }
                }
            }

            let res = tokio::select! {
                r = engine_clone
                    .session
                    .add_torrent(AddTorrent::from_url(&final_magnet), Some(opts)) => r,
                _ = rx => {
                    tracing::info!("Magnet {} was cancelled.", final_magnet);
                    return;
                }
            };

            if let Some(hash) = &info_hash_pending {
                let mut state = engine_clone.state.write().await;
                state.pending_magnets.remove(hash);
            }

            match res {
                Ok(res) => {
                    if let Some(handle) = res.into_handle() {
                        let info_hash = handle.info_hash().as_string();

                        // Persist to redb
                        {
                            let added_at = if auto_start { default_added_at() } else { 0 };
                            let record = TorrentRecord {
                                magnet_or_url: magnet_clone.clone(),
                                started: auto_start,
                                added_at,
                            };
                            if let Ok(json) = serde_json::to_string(&record)
                                && let Ok(write_txn) = engine_clone.db.begin_write()
                            {
                                if let Ok(mut table) = write_txn.open_table(TORRENTS_TABLE) {
                                    let _ = table.insert(info_hash.as_str(), json.as_str());
                                }
                                let _ = write_txn.commit();
                            }
                            let mut state = engine_clone.state.write().await;
                            state.torrent_started.insert(info_hash.clone(), auto_start);
                            state.torrent_added_at.insert(info_hash.clone(), added_at);
                            state
                                .torrent_magnets
                                .insert(info_hash.clone(), magnet_clone.clone());
                        }

                        // Create .info file in cache_dir (Go style)
                        let (cache_file, _) = engine_clone.get_cache_paths(&info_hash).await;
                        if !cache_file.exists() {
                            let _ = std::fs::write(cache_file, magnet_clone);
                        }

                        let _ = engine_clone.changed_tx.try_send(());
                    }
                }
                Err(e) => tracing::error!("Error adding magnet {}: {}", final_magnet, e),
            }
        });

        Ok(())
    }

    fn update_torrent_record(
        &self,
        info_hash: &str,
        started: bool,
        new_added_at: Option<i64>,
    ) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(TORRENTS_TABLE)?;
            let val_str = if let Some(val) = table.get(info_hash)? {
                val.value().to_string()
            } else {
                return Ok(());
            };

            if let Ok(mut record) = serde_json::from_str::<TorrentRecord>(&val_str) {
                record.started = started;
                if let Some(t) = new_added_at {
                    record.added_at = t;
                }
                let json = serde_json::to_string(&record)?;
                table.insert(info_hash, json.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    pub async fn get_metrics(&self) -> (u64, u64, u32) {
        let stats = self.session.stats_snapshot();
        let written = stats.uploaded_bytes;
        let read = stats.fetched_bytes;

        let active = self.session.with_torrents(|torrents| {
            torrents
                .filter(|(_, h)| !matches!(h.stats().state, TorrentStatsState::Paused))
                .count() as u32
        });

        (written, read, active)
    }

    pub async fn get_dht_stats(&self) -> (usize, usize) {
        if let Some(dht) = self.session.get_dht() {
            let stats = dht.stats();
            (stats.routing_table_size, 0)
        } else {
            (0, 0)
        }
    }

    pub async fn get_torrents(&self) -> Vec<Torrent> {
        let state_guard = self.state.read().await;
        let mut torrents = self.session.with_torrents(|managed_torrents| {
            let mut torrents = Vec::new();
            for (_, h) in managed_torrents {
                let stats = h.stats();
                let info_hash = h.info_hash().as_string();

                let mut files = vec![];
                let metadata_guard = h.metadata.load();
                if let Some(meta) = metadata_guard.as_ref() {
                    for f in &meta.file_infos {
                        files.push(serde_json::json!({
                            "Path": f.relative_filename.to_string_lossy(),
                            "Size": f.len,
                        }));
                    }
                }

                let name = h.name().unwrap_or_else(|| info_hash.clone());
                let size = stats.total_bytes as i64;
                let downloaded = stats.progress_bytes as i64;
                let percent = if size > 0 {
                    (downloaded as f32 / size as f32) * 100.0
                } else {
                    0.0
                };

                let added_at_ts = state_guard
                    .torrent_added_at
                    .get(&info_hash)
                    .copied()
                    .unwrap_or_else(default_added_at);
                let added_at = format_ago(added_at_ts);

                let magnet = state_guard
                    .torrent_magnets
                    .get(&info_hash)
                    .cloned()
                    .unwrap_or_default();

                let magnet_fallback = if magnet.starts_with("torrent_bytes:")
                    || magnet.starts_with("http://")
                    || magnet.starts_with("https://")
                    || magnet.is_empty()
                {
                    format!(
                        "magnet:?xt=urn:btih:{}&dn={}",
                        info_hash,
                        urlencoding::encode(&name)
                    )
                } else {
                    magnet.clone()
                };

                torrents.push(Torrent {
                    info_hash,
                    name,
                    magnet: magnet_fallback,
                    loaded: h.metadata.load().is_some(),
                    downloaded,
                    uploaded: stats.uploaded_bytes as i64,
                    size,
                    percent,
                    status: format!("{:?}", stats.state),
                    download_rate: stats
                        .live
                        .as_ref()
                        .map(|l| l.download_speed.mbps * 1024.0 * 1024.0)
                        .unwrap_or(0.0) as f32,
                    upload_rate: stats
                        .live
                        .as_ref()
                        .map(|l| l.upload_speed.mbps * 1024.0 * 1024.0)
                        .unwrap_or(0.0) as f32,
                    is_queueing: false,
                    is_seeding: stats.finished,
                    started: !matches!(stats.state, TorrentStatsState::Paused),
                    added_at,
                    peers_connected: stats
                        .live
                        .as_ref()
                        .map(|l| l.snapshot.peer_stats.live as u32)
                        .unwrap_or(0),
                    peers_total: stats
                        .live
                        .as_ref()
                        .map(|l| l.snapshot.peer_stats.seen as u32)
                        .unwrap_or(0),
                    peers_half_open: stats
                        .live
                        .as_ref()
                        .map(|l| l.snapshot.peer_stats.connecting as u32)
                        .unwrap_or(0),
                    peers_pending: stats
                        .live
                        .as_ref()
                        .map(|l| l.snapshot.peer_stats.queued as u32)
                        .unwrap_or(0),
                    seed_ratio: if downloaded > 0 {
                        stats.uploaded_bytes as f32 / downloaded as f32
                    } else {
                        0.0
                    },
                    added_at_ts,
                    files,
                });
            }
            torrents
        });

        // Add pending magnets
        for (hash, (magnet_url, added_ts, _)) in state_guard.pending_magnets.iter() {
            let name = if let Ok(m) = librqbit::Magnet::parse(magnet_url) {
                m.name.unwrap_or_else(|| hash.clone())
            } else {
                hash.clone()
            };

            torrents.push(Torrent {
                info_hash: hash.clone(),
                name,
                magnet: magnet_url.clone(),
                loaded: false,
                downloaded: 0,
                uploaded: 0,
                size: 0,
                percent: 0.0,
                status: "Resolving".to_string(), // UI placeholder state
                download_rate: 0.0,
                upload_rate: 0.0,
                is_queueing: false,
                is_seeding: false,
                started: true,
                added_at: format_ago(*added_ts),
                peers_connected: 0,
                peers_total: 0,
                peers_half_open: 0,
                peers_pending: 0,
                seed_ratio: 0.0,
                added_at_ts: *added_ts,
                files: vec![],
            });
        }

        torrents.sort_by_key(|t| std::cmp::Reverse(t.added_at_ts));
        torrents
    }

    pub async fn get_config(&self) -> Config {
        self.state.read().await.config.clone()
    }

    async fn fetch_trackers(&self, url: &str, force_refresh: bool) -> Vec<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if !force_refresh
            && let Ok(read_txn) = self.db.begin_read()
            && let Ok(table) = read_txn.open_table(TRACKERS_TABLE)
            && let Ok(Some(value)) = table.get(url)
            && let Ok(cached) = serde_json::from_str::<CachedTrackers>(value.value())
            && now - cached.updated_at < TRACKER_CACHE_TTL
        {
            tracing::debug!("Using cached trackers for {}", url);
            return cached.list;
        }

        tracing::info!("Fetching remote tracker list: {}", url);
        match build_http_client().get(url).send().await {
            Ok(resp) => {
                if let Ok(text) = resp.text().await {
                    let list: Vec<String> = text
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect();

                    if !list.is_empty() {
                        let cached = CachedTrackers {
                            list: list.clone(),
                            updated_at: now,
                        };
                        if let Ok(json) = serde_json::to_string(&cached)
                            && let Ok(write_txn) = self.db.begin_write()
                        {
                            if let Ok(mut table) = write_txn.open_table(TRACKERS_TABLE) {
                                let _ = table.insert(url, json.as_str());
                            }
                            let _ = write_txn.commit();
                        }
                    }
                    return list;
                }
            }
            Err(e) => tracing::error!("Failed to fetch trackers from {}: {}", url, e),
        }
        vec![]
    }

    pub async fn get_trackers(&self, force_refresh: bool) -> Vec<String> {
        let config = self.get_config().await;
        let mut trackers = Vec::new();
        for line in config.tracker_list.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(url) = line.strip_prefix("remote:") {
                trackers.extend(self.fetch_trackers(url, force_refresh).await);
            } else {
                trackers.push(line.to_string());
            }
        }
        trackers.sort();
        trackers.dedup();
        trackers
    }
}

fn format_ago(timestamp: i64) -> String {
    if timestamp == 0 {
        return "".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let diff = now - timestamp;
    if diff < 1 {
        return "just now".to_string();
    }
    if diff < 60 {
        return format!("{} seconds ago", diff);
    }
    let mins = diff / 60;
    if mins < 60 {
        return format!("{} minutes ago", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{} hours ago", hours);
    }
    let days = hours / 24;
    format!("{} days ago", days)
}
