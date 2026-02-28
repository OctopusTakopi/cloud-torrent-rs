use crate::engine::Engine;
use crate::engine::storage::Storage;
use crate::engine::types::CachedTrackers;
use crate::engine::utils::build_http_client;
use std::sync::Arc;

pub const TRACKER_CACHE_TTL: u64 = 3600 * 24; // 24 hours

pub async fn fetch_remote_trackers(
    storage: Arc<Storage>,
    url: &str,
    force_refresh: bool,
) -> Vec<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    if !force_refresh
        && let Ok(Some(cached)) = storage.load_trackers(url)
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
                    let _ = storage.save_trackers(url, &cached);
                }
                return list;
            }
        }
        Err(e) => tracing::error!("Failed to fetch trackers from {}: {}", url, e),
    }
    vec![]
}

pub async fn get_all_trackers(engine: &Engine, force_refresh: bool) -> Vec<String> {
    let config = engine.get_config().await;
    let mut trackers = Vec::new();
    let storage = engine.storage.clone();

    for line in config.tracker_list.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(url) = line.strip_prefix("remote:") {
            trackers.extend(fetch_remote_trackers(storage.clone(), url, force_refresh).await);
        } else {
            trackers.push(line.to_string());
        }
    }
    trackers.sort();
    trackers.dedup();
    trackers
}
