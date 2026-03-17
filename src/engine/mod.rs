pub mod storage;
pub mod trackers;
pub mod types;
pub mod utils;

use anyhow::{Context, Result};
pub use cloud_torrent_common::{Config, Torrent};
use librqbit::api::TorrentIdOrHash;
use librqbit::{AddTorrent, AddTorrentOptions, Session, SessionOptions, TorrentStatsState};
use redb::Database;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};

use crate::engine::storage::Storage;
pub use crate::engine::types::*;
pub use crate::engine::utils::*;

pub const CACHE_PREFIX: &str = "_CLDAUTOSAVED_";

#[derive(Clone)]
pub struct Engine {
    pub state: Arc<RwLock<EngineState>>,
    pub changed_tx: mpsc::Sender<()>,
    pub session: Arc<Session>,
    pub storage: Arc<Storage>,
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

        let db_path = cache_path.join("cloud-torrent.db");
        let db = Database::create(db_path)?;

        // Ensure tables exist
        {
            let write_txn = db.begin_write()?;
            {
                let _ = write_txn.open_table(TORRENTS_TABLE)?;
                let _ = write_txn.open_table(TRACKERS_TABLE)?;
                let _ = write_txn.open_table(RSS_TABLE)?;
            }
            write_txn.commit()?;
        }

        let mut options = SessionOptions {
            disable_dht: config.disable_trackers,
            ..Default::default()
        };

        let mut listen_opts = librqbit::ListenerOptions::default();
        listen_opts
            .listen_addr
            .set_port(config.incoming_port as u16);
        listen_opts.ipv4_only = config.disable_ipv6;
        if config.disable_utp {
            listen_opts.mode = librqbit::ListenerMode::TcpOnly;
        } else {
            listen_opts.mode = librqbit::ListenerMode::TcpAndUtp;
        }
        options.listen = Some(listen_opts);
        options.ipv4_only = config.disable_ipv6;

        if let Some(down) = parse_rate(&config.download_rate) {
            use std::num::NonZeroU32;
            options.ratelimits.download_bps = NonZeroU32::new(down);
        }
        if config.enable_upload {
            if let Some(up) = parse_rate(&config.upload_rate) {
                use std::num::NonZeroU32;
                options.ratelimits.upload_bps = NonZeroU32::new(up);
            }
        } else {
            use std::num::NonZeroU32;
            options.ratelimits.upload_bps = NonZeroU32::new(1);
        }

        let session = Session::new_with_opts(download_path.to_path_buf(), options).await?;
        let (tx, rx) = mpsc::channel(100);
        let storage = Arc::new(Storage::new(Arc::new(db)));

        let engine = Self {
            state: Arc::new(RwLock::new(EngineState {
                config,
                torrent_info: HashMap::new(),
                pending_magnets: HashMap::new(),
            })),
            changed_tx: tx,
            session: session.clone(),
            storage,
        };

        // Background manager loop
        let engine_clone = engine.clone();
        tokio::spawn(async move {
            engine_clone.manager_loop().await;
        });

        // Restore torrents from DB
        engine.restore_torrents().await?;

        Ok((engine, rx))
    }

    async fn manager_loop(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            let config = self.get_config().await;
            let mut available_download_budget = self.available_download_budget().await;
            let torrents = self.session.with_torrents(|torrents| {
                torrents.map(|(id, h)| (id, h.clone())).collect::<Vec<_>>()
            });

            let mut downloading_count: i32 = 0;
            let mut active_count: i32 = 0;
            let mut queue = vec![];

            for (id, h) in torrents {
                let stats = h.stats();
                let info_hash = h.info_hash().as_string();
                let remaining_bytes =
                    remaining_download_bytes(stats.total_bytes, stats.progress_bytes);
                let is_started = self
                    .state
                    .read()
                    .await
                    .torrent_info
                    .get(&info_hash)
                    .map(|i| i.started)
                    .unwrap_or(true);

                // Enable Seeding check
                if !config.enable_seeding
                    && stats.finished
                    && !matches!(stats.state, TorrentStatsState::Paused)
                {
                    tracing::info!("Torrent {} finished and seeding is disabled, stopping.", id);
                    let _ = self.session.pause(&h).await;
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
                        let _ = self.session.pause(&h).await;
                        continue;
                    }
                }

                if !matches!(stats.state, TorrentStatsState::Paused) {
                    if let Some(remaining_bytes) = remaining_bytes {
                        if remaining_bytes > available_download_budget {
                            tracing::warn!(
                                "Pausing torrent {} because it needs {} more bytes and only {} are available.",
                                id,
                                remaining_bytes,
                                available_download_budget
                            );
                            let _ = self.session.pause(&h).await;
                            continue;
                        }
                        available_download_budget =
                            available_download_budget.saturating_sub(remaining_bytes);
                    }

                    active_count += 1;
                    if !stats.finished {
                        downloading_count += 1;
                    }

                    // Max Active check
                    if config.max_active_torrents > 0 && active_count > config.max_active_torrents {
                        tracing::info!(
                            "Max active torrents reached ({}), pausing torrent {}.",
                            config.max_active_torrents,
                            id
                        );
                        let _ = self.session.pause(&h).await;
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
                        let _ = self.session.pause(&h).await;
                        downloading_count -= 1;
                        active_count -= 1;
                        continue;
                    }
                } else if !stats.finished && is_started {
                    queue.push((h, remaining_bytes));
                }
            }

            // If we have room, start torrents from the queue
            if (downloading_count < config.max_concurrent_task || config.max_concurrent_task == 0)
                && (active_count < config.max_active_torrents || config.max_active_torrents == 0)
            {
                for (h, remaining_bytes) in queue {
                    if config.max_concurrent_task > 0
                        && downloading_count >= config.max_concurrent_task
                    {
                        break;
                    }
                    if config.max_active_torrents > 0 && active_count >= config.max_active_torrents
                    {
                        break;
                    }
                    if let Some(remaining_bytes) = remaining_bytes {
                        if remaining_bytes > available_download_budget {
                            tracing::warn!(
                                "Skipping start for torrent {} because it needs {} more bytes and only {} are available.",
                                h.id(),
                                remaining_bytes,
                                available_download_budget
                            );
                            continue;
                        }
                        available_download_budget =
                            available_download_budget.saturating_sub(remaining_bytes);
                    }
                    tracing::info!("Starting queued torrent {}.", h.id());
                    let _ = self.session.unpause(&h).await;
                    downloading_count += 1;
                    active_count += 1;
                }
            }
        }
    }

    async fn restore_torrents(&self) -> Result<()> {
        let results = self.storage.load_torrents()?;
        let trackers = self.get_trackers(true).await;
        let restore_semaphore = Arc::new(tokio::sync::Semaphore::new(8));

        for (info_hash, record) in results {
            tracing::info!("Restoring torrent: {}", info_hash);
            {
                let mut state = self.state.write().await;
                state.torrent_info.insert(
                    info_hash.clone(),
                    TorrentInfo {
                        started: record.started,
                        added_at: record.added_at,
                        magnet: record.magnet_or_url.clone(),
                    },
                );
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
                    let info_hash = info_hash.clone();
                    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                    {
                        let mut state = self.state.write().await;
                        state.pending_magnets.insert(
                            info_hash.clone(),
                            PendingMagnet {
                                magnet_url: record.magnet_or_url.clone(),
                                added_at: record.added_at,
                                cancel_tx: Some(tx),
                            },
                        );
                    }
                    let restore_semaphore = restore_semaphore.clone();
                    tokio::spawn(async move {
                        let _permit = restore_semaphore.acquire_owned().await.unwrap();
                        let add_fut = engine_clone
                            .session
                            .add_torrent(AddTorrent::from_bytes(bytes), Some(opts));

                        tokio::select! {
                            res = add_fut => {
                                if let Err(e) = res {
                                    tracing::error!("Failed to restore torrent bytes {}: {}", info_hash, e);
                                }
                            },
                            _ = rx => {
                                tracing::info!("Restore {} was cancelled.", info_hash);
                            }
                        }
                        let mut state = engine_clone.state.write().await;
                        state.pending_magnets.remove(&info_hash);
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
            let info_hash = info_hash.clone();
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            {
                let mut state = self.state.write().await;
                state.pending_magnets.insert(
                    info_hash.clone(),
                    PendingMagnet {
                        magnet_url: record.magnet_or_url.clone(),
                        added_at: record.added_at,
                        cancel_tx: Some(tx),
                    },
                );
            }

            let restore_semaphore = restore_semaphore.clone();
            tokio::spawn(async move {
                let _permit = restore_semaphore.acquire_owned().await.unwrap();
                let add_fut = engine_clone
                    .session
                    .add_torrent(AddTorrent::from_url(&final_magnet), Some(opts));

                tokio::select! {
                    res = add_fut => {
                        if let Err(e) = res {
                            tracing::error!("Failed to restore torrent {}: {}", info_hash, e);
                        }
                    },
                    _ = rx => {
                        tracing::info!("Restore {} was cancelled.", info_hash);
                    }
                }
                let mut state = engine_clone.state.write().await;
                state.pending_magnets.remove(&info_hash);
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

        apply_ratelimits(&self.session, &new_config);

        // Save to file
        let yaml = serde_yaml::to_string(&new_config)?;
        std::fs::write("cloud-torrent.yaml", yaml)?;

        let mut state = self.state.write().await;
        state.config = new_config;
        let _ = self.changed_tx.try_send(());
        Ok(())
    }

    async fn get_cache_paths(&self, info_hash: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let state = self.state.read().await;
        let filename = format!("{}{}.info", CACHE_PREFIX, info_hash);
        let cache_file = Path::new(&state.config.cache_directory).join(&filename);
        let trash_file = Path::new(&state.config.trash_directory).join(&filename);
        (cache_file, trash_file)
    }

    pub async fn add_torrent_bytes(&self, bytes: Vec<u8>) -> Result<()> {
        let auto_start = self.state.read().await.config.auto_start;
        let opts = AddTorrentOptions {
            paused: true,
            overwrite: true,
            ..Default::default()
        };

        let res = self
            .session
            .add_torrent(AddTorrent::from_bytes(bytes.clone()), Some(opts))
            .await?;
        let handle = res.into_handle().context("failed to get torrent handle")?;
        let info_hash = handle.info_hash().as_string();

        if auto_start {
            let stats = handle.stats();
            if let Err(err) = self
                .ensure_disk_space_for_download(
                    stats.total_bytes as u64,
                    stats.progress_bytes as u64,
                )
                .await
            {
                let _ = self
                    .session
                    .delete(TorrentIdOrHash::Id(handle.id()), false)
                    .await;
                return Err(err);
            }
        }

        let added_at = if auto_start { default_added_at() } else { 0 };
        let record = TorrentRecord {
            magnet_or_url: format!("torrent_bytes:{}", hex::encode(&bytes)),
            started: auto_start,
            added_at,
        };

        self.storage.save_torrent(&info_hash, &record)?;

        {
            let mut state = self.state.write().await;
            state.torrent_info.insert(
                info_hash.clone(),
                TorrentInfo {
                    started: auto_start,
                    added_at,
                    magnet: record.magnet_or_url.clone(),
                },
            );
        }

        // Create .torrent file in cache_dir (Go style)
        let filename = format!("{}{}.torrent", CACHE_PREFIX, info_hash);
        let cache_file = Path::new(&self.state.read().await.config.cache_directory).join(&filename);
        if !cache_file.exists() {
            let _ = std::fs::write(cache_file, bytes);
        }

        if auto_start {
            let _ = self.session.unpause(&handle).await;
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
                let stats = h.stats();
                self.ensure_disk_space_for_download(stats.total_bytes, stats.progress_bytes)
                    .await?;
                let _ = self.session.unpause(&h).await;
                let now = default_added_at();
                {
                    let mut state = self.state.write().await;
                    if let Some(info) = state.torrent_info.get_mut(ih_hex) {
                        info.started = true;
                        info.added_at = now;
                        let record = TorrentRecord {
                            magnet_or_url: info.magnet.clone(),
                            started: true,
                            added_at: now,
                        };
                        let _ = self.storage.save_torrent(ih_hex, &record);
                    }
                }
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
                    if let Some(info) = state.torrent_info.get_mut(ih_hex) {
                        info.started = false;
                        info.added_at = 0;
                        let record = TorrentRecord {
                            magnet_or_url: info.magnet.clone(),
                            started: false,
                            added_at: 0,
                        };
                        let _ = self.storage.save_torrent(ih_hex, &record);
                    }
                }
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
                let _ = self.storage.remove_torrent(ih_hex);
                let (cache_file, trash_file) = self.get_cache_paths(ih_hex).await;
                if cache_file.exists() {
                    let _ = std::fs::rename(cache_file, trash_file);
                }
                let _ = self.session.delete(TorrentIdOrHash::Id(id), false).await;
            } else {
                let mut state = self.state.write().await;
                if let Some(pending) = state.pending_magnets.remove(ih_hex)
                    && let Some(tx) = pending.cancel_tx
                {
                    let _ = tx.send(());
                }
            }
            let _ = self.changed_tx.try_send(());
            return Ok(());
        }

        let config = self.get_config().await;
        let auto_start = config.auto_start;
        let opts = AddTorrentOptions {
            paused: true,
            overwrite: true,
            ..Default::default()
        };

        if magnet.starts_with("http://") || magnet.starts_with("https://") {
            tracing::info!("Fetching HTTP torrent from {}", magnet);
            let client = build_http_client();
            let resp = client.get(magnet).send().await?;
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

        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<Result<()>>();
        let mut cancel_rx = None;

        if let Some(hash) = &info_hash_pending {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let mut state = self.state.write().await;
            state.pending_magnets.insert(
                hash.clone(),
                PendingMagnet {
                    magnet_url: magnet_clone.clone(),
                    added_at: default_added_at(),
                    cancel_tx: Some(tx),
                },
            );
            cancel_rx = Some(rx);
            let _ = self.changed_tx.try_send(());
        }

        tokio::spawn(async move {
            let mut final_magnet = magnet_clone.clone();

            if magnet_clone.starts_with("magnet:") {
                let has_trackers = final_magnet.contains("&tr=");
                if config.always_add_trackers || !has_trackers {
                    let trackers =
                        crate::engine::trackers::get_all_trackers(&engine_clone, false).await;
                    for tr in trackers {
                        let encoded_tr = urlencoding::encode(&tr);
                        if !final_magnet.contains(&encoded_tr.to_string()) {
                            final_magnet.push_str("&tr=");
                            final_magnet.push_str(&encoded_tr);
                        }
                    }
                }
            }

            let add_result = async {
                match engine_clone
                    .session
                    .add_torrent(AddTorrent::from_url(&final_magnet), Some(opts))
                    .await
                {
                    Ok(res) => {
                        if let Some(handle) = res.into_handle() {
                            let info_hash = handle.info_hash().as_string();
                            let added_at = if auto_start { default_added_at() } else { 0 };

                            if auto_start {
                                let stats = handle.stats();
                                if let Err(err) = engine_clone
                                    .ensure_disk_space_for_download(
                                        stats.total_bytes,
                                        stats.progress_bytes,
                                    )
                                    .await
                                {
                                    let _ = engine_clone
                                        .session
                                        .delete(TorrentIdOrHash::Id(handle.id()), false)
                                        .await;
                                    tracing::warn!(
                                        "Rejecting magnet {} because of insufficient disk space: {}",
                                        magnet_clone,
                                        err
                                    );
                                    return Err(err);
                                }
                            }

                            let record = TorrentRecord {
                                magnet_or_url: magnet_clone.clone(),
                                started: auto_start,
                                added_at,
                            };
                            engine_clone.storage.save_torrent(&info_hash, &record)?;

                            {
                                let mut state = engine_clone.state.write().await;
                                state.torrent_info.insert(
                                    info_hash.clone(),
                                    TorrentInfo {
                                        started: auto_start,
                                        added_at,
                                        magnet: magnet_clone.clone(),
                                    },
                                );
                            }

                            let (cache_file, _) = engine_clone.get_cache_paths(&info_hash).await;
                            if !cache_file.exists() {
                                tokio::fs::write(cache_file, &magnet_clone).await?;
                            }

                            if auto_start {
                                let _ = engine_clone.session.unpause(&handle).await;
                            }
                            let _ = engine_clone.changed_tx.try_send(());
                            Ok(())
                        } else {
                            Err(anyhow::anyhow!("failed to get torrent handle"))
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error adding magnet {}: {}", final_magnet, e);
                        Err(e)
                    }
                }
            };
            let result = if let Some(rx) = cancel_rx {
                tokio::select! {
                    res = add_result => res,
                    _ = rx => {
                        tracing::info!("Magnet {} was cancelled.", final_magnet);
                        Err(anyhow::anyhow!("magnet add was cancelled"))
                    }
                }
            } else {
                add_result.await
            };

            if let Some(hash) = &info_hash_pending {
                let mut state = engine_clone.state.write().await;
                state.pending_magnets.remove(hash);
                let _ = engine_clone.changed_tx.try_send(());
            }

            let _ = result_tx.send(result);
        });

        result_rx
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("magnet add task terminated unexpectedly")))
    }

    pub async fn get_metrics(&self) -> (u64, u64, u32) {
        let stats = self.session.stats_snapshot();
        let written = stats.counters.uploaded_bytes;
        let read = stats.counters.fetched_bytes;

        let active = self.session.with_torrents(|torrents| {
            torrents
                .filter(|(_, h)| !matches!(h.stats().state, TorrentStatsState::Paused))
                .count() as u32
        });

        (written, read, active)
    }

    pub async fn get_dht_stats(&self) -> (usize, usize) {
        if let Some(dht) = self.session.get_dht() {
            let mut v4 = 0;
            let mut v6 = 0;
            dht.with_routing_tables(|v4_rt, v6_rt| {
                v4 = v4_rt.len();
                v6 = v6_rt.len();
            });
            (v4, v6)
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

                let info = state_guard.torrent_info.get(&info_hash);
                let added_at_ts = info.map(|i| i.added_at).unwrap_or_else(default_added_at);
                let added_at = format_ago(added_at_ts);
                let magnet = info.map(|i| i.magnet.clone()).unwrap_or_default();

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
                    magnet
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
                        .map(|l| l.snapshot.peer_stats.live)
                        .unwrap_or(0),
                    peers_total: stats
                        .live
                        .as_ref()
                        .map(|l| l.snapshot.peer_stats.seen)
                        .unwrap_or(0),
                    peers_half_open: stats
                        .live
                        .as_ref()
                        .map(|l| l.snapshot.peer_stats.connecting)
                        .unwrap_or(0),
                    peers_pending: stats
                        .live
                        .as_ref()
                        .map(|l| l.snapshot.peer_stats.queued)
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
        for (hash, pending) in state_guard.pending_magnets.iter() {
            let name = if let Ok(m) = librqbit::Magnet::parse(&pending.magnet_url) {
                m.name.unwrap_or_else(|| hash.clone())
            } else {
                hash.clone()
            };

            torrents.push(Torrent {
                info_hash: hash.clone(),
                name,
                magnet: pending.magnet_url.clone(),
                loaded: false,
                downloaded: 0,
                uploaded: 0,
                size: 0,
                percent: 0.0,
                status: "Resolving".to_string(),
                download_rate: 0.0,
                upload_rate: 0.0,
                is_queueing: false,
                is_seeding: false,
                started: true,
                added_at: format_ago(pending.added_at),
                peers_connected: 0,
                peers_total: 0,
                peers_half_open: 0,
                peers_pending: 0,
                seed_ratio: 0.0,
                added_at_ts: pending.added_at,
                files: vec![],
            });
        }

        torrents.sort_by_key(|t| std::cmp::Reverse(t.added_at_ts));
        torrents
    }

    pub async fn get_config(&self) -> Config {
        self.state.read().await.config.clone()
    }

    pub async fn get_trackers(&self, force_refresh: bool) -> Vec<String> {
        crate::engine::trackers::get_all_trackers(self, force_refresh).await
    }

    async fn available_download_budget(&self) -> u64 {
        let config = self.get_config().await;
        available_space_for_path(Path::new(&config.download_directory))
            .saturating_sub(DISK_SPACE_RESERVE_BYTES)
    }

    async fn active_download_reservation(&self) -> u64 {
        self.session.with_torrents(|torrents| {
            torrents
                .filter_map(|(_, h)| {
                    let stats = h.stats();
                    if matches!(stats.state, TorrentStatsState::Paused) {
                        None
                    } else {
                        remaining_download_bytes(stats.total_bytes, stats.progress_bytes)
                    }
                })
                .sum()
        })
    }

    async fn available_start_budget(&self) -> u64 {
        self.available_download_budget()
            .await
            .saturating_sub(self.active_download_reservation().await)
    }

    async fn ensure_disk_space_for_download(
        &self,
        total_bytes: u64,
        progress_bytes: u64,
    ) -> Result<()> {
        if let Some(required_bytes) = remaining_download_bytes(total_bytes, progress_bytes) {
            let free_bytes = self.available_start_budget().await;
            if required_bytes > free_bytes {
                return Err(EngineError::InsufficientStorage(format_storage_error(
                    required_bytes,
                    free_bytes,
                ))
                .into());
            }
        }
        Ok(())
    }
}
