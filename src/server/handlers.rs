use crate::engine::types::EngineError;
use crate::server::scraper::search_scraper;
use crate::server::state::AppState;
use crate::server::types::{
    AppError, AppResult, FileNode, MagnetQuery, RssLoadRequest, RssQuery, SearchQuery,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response, sse::Event, sse::Sse},
};
use cloud_torrent_common::{GlobalState, Torrent};
use futures::stream;
use std::convert::Infallible;
use std::sync::Arc;
use sysinfo::Disks;
use tokio::process::Command;
use tokio_util::io::ReaderStream;
use tower::ServiceExt;

pub async fn sync_handler(
    State(state): State<Arc<AppState>>,
    State(broadcast_tx): State<tokio::sync::broadcast::Sender<()>>,
) -> impl IntoResponse {
    let broadcast_rx = broadcast_tx.subscribe();
    let stream = stream::unfold(
        (state, 1, broadcast_rx),
        |(state, mut version, mut broadcast_rx)| async move {
            let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            if version > 1 {
                tokio::select! {
                    _ = broadcast_rx.recv() => {},
                    _ = heartbeat.tick() => {},
                }
            }

            let current_state = get_global_state(&state).await;
            version += 1;
            let event = Event::default().data(serde_json::to_string(&current_state).unwrap());
            Some((Ok::<_, Infallible>(event), (state, version, broadcast_rx)))
        },
    );

    Sse::new(stream).into_response()
}

pub async fn get_global_state(state: &AppState) -> GlobalState {
    let stats_val = get_system_stats(state).await;
    let rss_snapshot = state.rss.snapshot().await;
    GlobalState {
        use_queue: false,
        latest_rss_guid: rss_snapshot.latest_guid.clone(),
        rss_last_error: rss_snapshot.last_error,
        torrents: state.engine.get_torrents().await,
        users: std::collections::HashMap::new(),
        stats: serde_json::from_value(stats_val).unwrap(),
    }
}

pub async fn get_system_stats(state: &AppState) -> serde_json::Value {
    let mut sys = state.sys.lock().await;

    // Only refresh what's needed for better performance
    sys.refresh_cpu_usage();
    sys.refresh_memory();

    let app_memory = sysinfo::get_current_pid()
        .ok()
        .and_then(|pid| sys.process(pid))
        .map(|p| p.memory())
        .unwrap_or(0);

    let engine = &state.engine;
    let config = engine.get_config().await;
    let download_dir = std::path::Path::new(&config.download_directory);
    let abs_path = if download_dir.is_absolute() {
        download_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(download_dir)
    };
    let abs_path = std::fs::canonicalize(&abs_path).unwrap_or(abs_path);

    let mut disk_free = 0;
    let mut disk_used_percent = 0.0;
    let disks = Disks::new_with_refreshed_list();
    let mut best_match: Option<&sysinfo::Disk> = None;
    for disk in &disks {
        if abs_path.starts_with(disk.mount_point()) {
            match best_match {
                Some(m)
                    if disk.mount_point().as_os_str().len() > m.mount_point().as_os_str().len() =>
                {
                    best_match = Some(disk);
                }
                None => best_match = Some(disk),
                _ => {}
            }
        }
    }
    if best_match.is_none() {
        best_match = disks
            .iter()
            .find(|d| d.mount_point() == std::path::Path::new("/"));
    }
    if let Some(disk) = best_match {
        disk_free = disk.available_space();
        let total = disk.total_space();
        if total > 0 {
            disk_used_percent = (total - disk_free) as f64 / total as f64 * 100.0;
        }
    }

    let (written, read, active) = engine.get_metrics().await;
    let (nodes4, nodes6) = engine.get_dht_stats().await;

    serde_json::json!({
        "System": {
            "Cpu": sys.global_cpu_usage(),
            "MemUsedPercent": (sys.used_memory() as f64 / sys.total_memory() as f64) * 100.0,
            "DiskUsedPercent": disk_used_percent,
            "DiskFree": disk_free,
            "AppMemory": app_memory,
            "ActiveTasks": active,
            "Version": env!("CARGO_PKG_VERSION"),
            "Dht": {
                "Nodes4": nodes4,
                "Nodes6": nodes6,
            }
        },
        "ConnStat": {
            "BytesWrittenData": written,
            "BytesReadUsefulData": read,
        }
    })
}

pub async fn api_rss(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RssQuery>,
) -> Json<cloud_torrent_common::RssSnapshot> {
    let snapshot = if params.refresh {
        state.rss.refresh(&state.engine).await.snapshot
    } else {
        state.rss.snapshot().await
    };
    Json(snapshot)
}

pub async fn api_rss_load(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RssLoadRequest>,
) -> AppResult<Json<cloud_torrent_common::RssSnapshot>> {
    state
        .engine
        .add_magnet(&req.load_url)
        .await
        .map_err(map_engine_error)?;
    let snapshot = state
        .rss
        .mark_item_loaded(&state.engine, &req.item_id)
        .await
        .map_err(map_engine_error)?;
    Ok(Json(snapshot))
}

pub async fn api_files(State(state): State<Arc<AppState>>) -> AppResult<Json<FileNode>> {
    let config = state.engine.get_config().await;
    let root = list_files_recursive(&config.download_directory).await?;
    Ok(Json(root))
}

#[async_recursion::async_recursion]
async fn list_files_recursive(path: &str) -> AppResult<FileNode> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| AppError::NotFound(e.to_string()))?;
    let name = std::path::Path::new(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let mut size = metadata.len();
    let mut children = None;

    if metadata.is_dir() {
        size = 0;
        let mut child_nodes = vec![];
        if let Ok(mut entries) = tokio::fs::read_dir(path).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let file_name = entry.file_name();
                if file_name.to_string_lossy().starts_with('.') {
                    continue;
                }
                if let Ok(child) = list_files_recursive(&entry.path().to_string_lossy()).await {
                    size += child.size;
                    child_nodes.push(child);
                }
            }
        }
        children = Some(child_nodes);
    }

    Ok(FileNode {
        name,
        size,
        modified: chrono::DateTime::<chrono::Utc>::from(
            metadata.modified().unwrap_or(std::time::SystemTime::now()),
        )
        .to_rfc3339(),
        children,
    })
}

pub async fn serve_download(
    Path(path): Path<String>,
    State(state): State<Arc<AppState>>,
    req: Request,
) -> impl IntoResponse {
    let config = state.engine.get_config().await;
    let full_path =
        std::path::Path::new(&config.download_directory).join(path.trim_start_matches('/'));
    if !full_path.exists() {
        return (StatusCode::NOT_FOUND, "File not found").into_response();
    }

    if full_path.is_dir() {
        let zip_name = format!(
            "{}.zip",
            full_path.file_name().unwrap_or_default().to_string_lossy()
        );

        let child = Command::new("zip")
            .arg("-0") // Store only (pack not compress)
            .arg("-r") // Recursive
            .arg("-") // Output to stdout
            .arg(".") // Current directory
            .current_dir(&full_path)
            .stdout(std::process::Stdio::piped())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to spawn zip command: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Error zipping directory")
                    .into_response();
            }
        };

        let stdout = child.stdout.take().unwrap();

        // Ensure child process is cleaned up
        tokio::spawn(async move {
            let _ = child.wait().await;
        });

        return Response::builder()
            .header(header::CONTENT_TYPE, "application/zip")
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", zip_name),
            )
            .body(axum::body::Body::from_stream(ReaderStream::new(stdout)))
            .unwrap()
            .into_response();
    }

    match tower_http::services::ServeFile::new(&full_path)
        .oneshot(req)
        .await
    {
        Ok(res) => res.into_response(),
        Err(err) => {
            tracing::error!("Error serving file: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error serving file").into_response()
        }
    }
}

pub async fn delete_download(
    Path(path): Path<String>,
    State(state): State<Arc<AppState>>,
) -> StatusCode {
    let config = state.engine.get_config().await;
    let full_path =
        std::path::Path::new(&config.download_directory).join(path.trim_start_matches('/'));

    let ok = if full_path.is_dir() {
        std::fs::remove_dir_all(&full_path).is_ok()
    } else {
        std::fs::remove_file(&full_path).is_ok()
    };

    if ok {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

pub async fn api_magnet_get(
    State(state): State<Arc<AppState>>,
    Query(params): Query<MagnetQuery>,
) -> AppResult<impl IntoResponse> {
    state
        .engine
        .add_magnet(&params.m)
        .await
        .map_err(map_engine_error)?;
    Ok((StatusCode::OK, "Magnet added"))
}

pub async fn api_magnet_post(
    State(state): State<Arc<AppState>>,
    body: String,
) -> AppResult<impl IntoResponse> {
    let lines: Vec<&str> = body
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    let mut errors = Vec::new();
    for line in lines {
        if let Err(err) = state.engine.add_magnet(line).await {
            errors.push(err);
        }
    }
    if let Some(err) = errors.into_iter().next() {
        Err(map_engine_error(err))
    } else {
        Ok((StatusCode::OK, "OK"))
    }
}

pub async fn api_torrent_post(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> AppResult<impl IntoResponse> {
    state
        .engine
        .add_torrent_bytes(body.to_vec())
        .await
        .map_err(map_engine_error)?;
    Ok((StatusCode::OK, "OK"))
}

pub async fn api_configure_get(
    State(state): State<Arc<AppState>>,
) -> Json<cloud_torrent_common::Config> {
    Json(state.engine.get_config().await)
}

pub async fn api_configure_post(
    State(state): State<Arc<AppState>>,
    Json(config): Json<cloud_torrent_common::Config>,
) -> StatusCode {
    if let Err(e) = state.engine.update_config(config).await {
        tracing::error!("Failed to update config: {}", e);
        return StatusCode::INTERNAL_SERVER_ERROR;
    }

    let _ = state.rss.refresh(&state.engine).await;
    StatusCode::OK
}

pub async fn api_torrents(State(state): State<Arc<AppState>>) -> Json<Vec<Torrent>> {
    Json(state.engine.get_torrents().await)
}

pub async fn api_stat(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(get_system_stats(state.as_ref()).await)
}

pub async fn api_search(
    Query(params): Query<SearchQuery>,
    State(state): State<Arc<AppState>>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    let provider = params
        .provider
        .unwrap_or_else(|| "thepiratebay".to_string());
    let scraper_url = state.engine.get_config().await.scraper_url;

    let config_json = crate::server::scraper::load_scraper_config(&scraper_url, false).await;

    let results = search_scraper(&params.query, Some(provider), &config_json).await?;
    Ok(Json(results))
}

pub async fn api_search_providers(
    State(state): State<Arc<AppState>>,
) -> Json<std::collections::HashMap<String, serde_json::Value>> {
    let scraper_url = state.engine.get_config().await.scraper_url;
    let config = crate::server::scraper::load_scraper_config(&scraper_url, false).await;

    let providers = config
        .into_iter()
        .filter(|(k, _)| !k.ends_with("/item"))
        .collect();

    Json(providers)
}

fn map_engine_error(err: anyhow::Error) -> AppError {
    if let Some(engine_err) = err.downcast_ref::<EngineError>() {
        return match engine_err {
            EngineError::InsufficientStorage(message) => {
                AppError::InsufficientStorage(message.clone())
            }
            _ => AppError::Internal(err),
        };
    }
    AppError::Internal(err)
}

pub async fn static_handler(
    State(_state): State<Arc<AppState>>,
    req: Request,
) -> impl IntoResponse {
    let path = req.uri().path().trim_start_matches('/');

    if path.is_empty() || path == "index.html" {
        return serve_asset("index.html");
    }

    match serve_asset(path) {
        resp if resp.status() == StatusCode::OK => resp,
        _ => serve_asset("index.html"), // SPA fallback
    }
}

fn serve_asset(path: &str) -> Response {
    use crate::server::Assets;
    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(axum::body::Body::from(content.data))
                .unwrap()
        }
        None => (StatusCode::NOT_FOUND, "Not Found").into_response(),
    }
}

pub async fn sync_ws_handler(
    ws: axum::extract::ws::WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    State(broadcast_tx): State<tokio::sync::broadcast::Sender<()>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state, broadcast_tx))
}

async fn handle_socket(
    mut socket: axum::extract::ws::WebSocket,
    state: Arc<AppState>,
    broadcast_tx: tokio::sync::broadcast::Sender<()>,
) {
    let mut broadcast_rx = broadcast_tx.subscribe();
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(2));

    loop {
        let current_state = get_global_state(&state).await;
        if let Ok(json) = serde_json::to_string(&current_state)
            && socket
                .send(axum::extract::ws::Message::text(json))
                .await
                .is_err()
        {
            break;
        }

        tokio::select! {
            _ = broadcast_rx.recv() => {},
            _ = heartbeat.tick() => {},
        }
    }
}
