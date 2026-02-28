use crate::engine::{Engine, build_http_client};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use base64::Engine as Base64Engine;
use cloud_torrent_common::GlobalState;
use core::net::SocketAddr;
use futures::stream;
use rust_embed::RustEmbed;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::sync::Arc;
use sysinfo::{Disks, System};
use tower::ServiceExt;
use tower_http::cors::CorsLayer;
// Use a local definition of ServeDir if needed, but we'll use our embedded assets
// use tower_http::services::ServeDir;

/// Scraper config embedded at compile time; filesystem file takes precedence at runtime.
const SCRAPER_CONFIG_EMBEDDED: &str = include_str!("../../scraper-config.json");

fn load_scraper_config_sync() -> String {
    std::fs::read_to_string("scraper-config.json")
        .unwrap_or_else(|_| SCRAPER_CONFIG_EMBEDDED.to_string())
}

async fn load_scraper_config(remote_url: &str) -> String {
    if !remote_url.is_empty()
        && let Ok(resp) = build_http_client().get(remote_url).send().await
        && let Ok(text) = resp.text().await
        && !text.trim().is_empty()
    {
        return text;
    }
    load_scraper_config_sync()
}
#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct Assets;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    tcp_addr: Option<SocketAddr>,
    unix_path: Option<String>,
    unix_perm: String,
    _title: String,
    engine: Engine,
    mut changed_rx: tokio::sync::mpsc::Receiver<()>,
    auth: Option<String>,
    cert_path: Option<String>,
    key_path: Option<String>,
) -> anyhow::Result<()> {
    let (broadcast_tx, _) = tokio::sync::broadcast::channel(100);

    let expected_auth = auth.map(|a| {
        let b64 = base64::engine::general_purpose::STANDARD.encode(a.as_bytes());
        format!("Basic {}", b64)
    });

    let state = Arc::new(AppState {
        engine,
        expected_auth,
    });

    let broadcast_tx_cloned = broadcast_tx.clone();
    tokio::spawn(async move {
        while changed_rx.recv().await.is_some() {
            let _ = broadcast_tx_cloned.send(());
        }
    });

    type SharedState = (Arc<AppState>, tokio::sync::broadcast::Sender<()>);
    let app: Router<()> = Router::<SharedState>::new()
        .route("/sync", get(sync_handler))
        .route("/sync/ws", get(sync_ws_handler))
        .route("/rss", get(api_rss))
        .route("/api/magnet", get(api_magnet_get).post(api_magnet_post))
        .route(
            "/api/configure",
            get(api_configure_get).post(api_configure_post),
        )
        .route("/api/torrents", get(api_torrents))
        .route("/api/torrent", post(api_torrent_post))
        .route("/api/stat", get(api_stat))
        .route("/api/files", get(api_files))
        .route("/api/search", get(api_search))
        .route("/api/searchproviders", get(api_search_providers))
        .route(
            "/download/{*path}",
            get(serve_download).delete(delete_download),
        )
        .fallback(static_handler)
        .with_state((state.clone(), broadcast_tx))
        .layer(CorsLayer::permissive())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    #[cfg(not(unix))]
    if unix_path.is_some() {
        return Err(anyhow::anyhow!(
            "Unix sockets are not supported on this platform"
        ));
    }

    #[cfg(unix)]
    if let Some(sock_path) = unix_path {
        // Remove existing socket file if present
        let _ = std::fs::remove_file(&sock_path);
        tracing::info!("Listening on unix:{}", sock_path);
        let listener = std::os::unix::net::UnixListener::bind(&sock_path)
            .map_err(|e| anyhow::anyhow!("Failed to bind unix socket {}: {}", sock_path, e))?;

        // Apply file permissions
        if let Ok(mode) = u32::from_str_radix(unix_perm.trim_start_matches('0'), 8) {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode);
            if let Err(e) = std::fs::set_permissions(&sock_path, perms) {
                tracing::warn!("Failed to set socket permissions: {}", e);
            } else {
                tracing::info!("Unix socket permissions set to {}", unix_perm);
            }
        }

        axum_server::from_unix(listener)
            .map_err(|e| anyhow::anyhow!("unix listener error: {}", e))?
            .serve(app.into_make_service())
            .await?;
    } else if let Some(addr) = tcp_addr {
        if let (Some(cert), Some(key)) = (cert_path, key_path) {
            tracing::info!("Listening on {} (TLS)", addr);
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to load TLS config: {}", e))?;
            axum_server::bind_rustls(addr, config)
                .serve(app.into_make_service())
                .await?;
        } else {
            tracing::info!("Listening on {}", addr);
            axum_server::bind(addr)
                .serve(app.into_make_service())
                .await?;
        }
    } else {
        return Err(anyhow::anyhow!("No listen address configured"));
    }
    Ok(())
}

struct AppState {
    engine: Engine,
    expected_auth: Option<String>,
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, Response> {
    if let Some(expected) = &state.expected_auth {
        let auth_header = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok());

        if auth_header != Some(expected) {
            return Err((
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"Restricted\"")],
                "Unauthorized",
            )
                .into_response());
        }
    }
    Ok(next.run(req).await)
}

async fn sync_handler(
    State((state, broadcast_tx)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
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

async fn sync_ws_handler(
    ws: WebSocketUpgrade,
    State((state, broadcast_tx)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state, broadcast_tx))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: Arc<AppState>,
    broadcast_tx: tokio::sync::broadcast::Sender<()>,
) {
    let mut broadcast_rx = broadcast_tx.subscribe();
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(2));

    loop {
        let current_state = get_global_state(&state).await;
        if let Ok(json) = serde_json::to_string(&current_state)
            && socket.send(Message::text(json)).await.is_err()
        {
            break;
        }

        tokio::select! {
            _ = broadcast_rx.recv() => {},
            _ = heartbeat.tick() => {},
        }
    }
}

async fn get_global_state(state: &AppState) -> GlobalState {
    let stats_val = get_system_stats(&state.engine).await;
    GlobalState {
        use_queue: false,
        latest_rss_guid: "".to_string(),
        torrents: state.engine.get_torrents().await,
        users: std::collections::HashMap::new(),
        stats: serde_json::from_value(stats_val).unwrap(),
    }
}

async fn get_system_stats(engine: &Engine) -> serde_json::Value {
    let mut sys = System::new_all();
    sys.refresh_all();

    let app_memory = sysinfo::get_current_pid()
        .ok()
        .and_then(|pid| sys.process(pid))
        .map(|p| p.memory())
        .unwrap_or(0);

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

async fn api_rss(
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
) -> Json<Vec<serde_json::Value>> {
    let config = state.engine.get_config().await;
    if config.rss_url.is_empty() {
        return Json(vec![]);
    }
    if let Ok(resp) = build_http_client().get(&config.rss_url).send().await
        && let Ok(_text) = resp.text().await
    {
        // Parsing would go here
    }
    Json(vec![])
}

async fn api_files(
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
) -> Json<serde_json::Value> {
    let config = state.engine.get_config().await;
    let root =
        list_files_recursive(&config.download_directory).unwrap_or_else(|_| serde_json::json!({}));
    Json(root)
}

fn list_files_recursive(path: &str) -> anyhow::Result<serde_json::Value> {
    let metadata = std::fs::metadata(path)?;
    let name = std::path::Path::new(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let mut size = metadata.len();
    let mut children = vec![];

    if metadata.is_dir() {
        size = 0;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                if file_name.to_string_lossy().starts_with('.') {
                    continue;
                }
                if let Ok(child) = list_files_recursive(&entry.path().to_string_lossy()) {
                    size += child.get("Size").and_then(|v| v.as_u64()).unwrap_or(0);
                    children.push(child);
                }
            }
        }
    }

    let mut node = serde_json::json!({
        "Name": name,
        "Size": size,
        "Modified": chrono::DateTime::<chrono::Utc>::from(metadata.modified()?).to_rfc3339(),
    });
    if metadata.is_dir() {
        node.as_object_mut()
            .unwrap()
            .insert("Children".to_string(), serde_json::json!(children));
    }
    Ok(node)
}

async fn serve_download(
    axum::extract::Path(path): axum::extract::Path<String>,
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
    req: Request,
) -> impl IntoResponse {
    let config = state.engine.get_config().await;
    let full_path =
        std::path::Path::new(&config.download_directory).join(path.trim_start_matches('/'));
    if !full_path.exists() {
        return (StatusCode::NOT_FOUND, "File not found").into_response();
    }

    if full_path.is_dir() {
        let path_clone = full_path.to_path_buf();
        let zip_name = format!(
            "{}.zip",
            full_path.file_name().unwrap_or_default().to_string_lossy()
        );

        let zip_data = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            {
                let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
                if let Err(e) = add_dir_to_zip_sync(&mut zip, &path_clone, "") {
                    tracing::error!("Zip error: {}", e);
                }
                let _ = zip.finish();
            }
            buf
        })
        .await
        .unwrap_or_default();

        return Response::builder()
            .header(header::CONTENT_TYPE, "application/zip")
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", zip_name),
            )
            .body(axum::body::Body::from(zip_data))
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

fn add_dir_to_zip_sync(
    zip: &mut zip::ZipWriter<std::io::Cursor<&mut Vec<u8>>>,
    path: &std::path::Path,
    prefix: &str,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        let entry_name = entry.file_name().to_string_lossy().to_string();
        let zip_path = if prefix.is_empty() {
            entry_name
        } else {
            format!("{}/{}", prefix, entry_name)
        };
        if entry_path.is_dir() {
            zip.add_directory(&zip_path, zip::write::SimpleFileOptions::default())?;
            add_dir_to_zip_sync(zip, &entry_path, &zip_path)?;
        } else {
            zip.start_file(&zip_path, zip::write::SimpleFileOptions::default())?;
            let mut f = std::fs::File::open(entry_path)?;
            let mut buffer = Vec::new();
            f.read_to_end(&mut buffer)?;
            zip.write_all(&buffer)?;
        }
    }
    Ok(())
}

async fn delete_download(
    axum::extract::Path(path): axum::extract::Path<String>,
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
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

async fn api_magnet_get(
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let m = params.get("m").cloned().unwrap_or_default();
    let res = state.engine.add_magnet(&m).await;
    if res.is_ok() {
        (StatusCode::OK, "Magnet added").into_response()
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, "Error adding magnet").into_response()
    }
}

async fn api_magnet_post(
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
    body: String,
) -> impl IntoResponse {
    let _ = state.engine.add_magnet(&body).await;
    (StatusCode::OK, "OK").into_response()
}

async fn api_torrent_post(
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let _ = state.engine.add_torrent_bytes(body.to_vec()).await;
    (StatusCode::OK, "OK").into_response()
}

async fn api_configure_get(
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
) -> Json<cloud_torrent_common::Config> {
    Json(state.engine.get_config().await)
}

async fn api_configure_post(
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
    Json(config): Json<cloud_torrent_common::Config>,
) -> StatusCode {
    if let Err(e) = state.engine.update_config(config).await {
        tracing::error!("Failed to update config: {}", e);
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    StatusCode::OK
}

async fn api_torrents(
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
) -> Json<Vec<cloud_torrent_common::Torrent>> {
    Json(state.engine.get_torrents().await)
}

async fn api_stat(
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
) -> Json<serde_json::Value> {
    Json(get_system_stats(&state.engine).await)
}

async fn api_search(
    Query(params): Query<std::collections::HashMap<String, String>>,
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
) -> Json<Vec<serde_json::Value>> {
    let query = params.get("query").cloned().unwrap_or_default();
    let provider = params
        .get("provider")
        .cloned()
        .unwrap_or_else(|| "torrentgalaxy".to_string());
    let scraper_url = state.engine.get_config().await.scraper_url;

    let results = search_scraper(&provider, &query, &scraper_url)
        .await
        .unwrap_or_default();
    Json(results)
}

async fn search_scraper(
    provider: &str,
    query: &str,
    scraper_url: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let config_str = load_scraper_config(scraper_url).await;
    let config: serde_json::Value = serde_json::from_str(&config_str)?;
    let p_conf = config
        .get(provider)
        .ok_or_else(|| anyhow::anyhow!("Provider not found"))?;

    let url_tpl = p_conf
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("No URL"))?;
    let url = url_tpl.replace("{{query}}", &urlencoding::encode(query));
    let url = url.replace("{{page:0}}", "0").replace("{{page:1}}", "1");

    let client = build_http_client();

    let resp = client.get(&url).send().await?.text().await?;

    let is_json = p_conf
        .get("json")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let res_conf = p_conf
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("No result config"))?;
    let mut results = vec![];

    if is_json {
        let json_resp: serde_json::Value = serde_json::from_str(&resp)?;
        if let Some(items) = json_resp.as_array() {
            for item_val in items {
                let mut mapped_item = serde_json::Map::new();
                if let Some(res_obj) = res_conf.as_object() {
                    for (key, val) in res_obj {
                        if let Some(field_name) = val.as_str()
                            && let Some(v) = item_val.get(field_name)
                        {
                            let v_str = match v {
                                serde_json::Value::Number(n) => n.to_string(),
                                serde_json::Value::String(s) => s.clone(),
                                _ => v.to_string(),
                            };
                            mapped_item.insert(key.clone(), serde_json::Value::String(v_str));
                        }
                    }
                }

                // Special handling for TPB/apibay style magnet construction
                if !mapped_item.contains_key("magnet")
                    && let Some(ih) = mapped_item.get("info_hash").and_then(|v| v.as_str())
                {
                    let name = mapped_item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let magnet = format!(
                        "magnet:?xt=urn:btih:{}&dn={}",
                        ih,
                        urlencoding::encode(name)
                    );
                    mapped_item.insert("magnet".to_string(), serde_json::Value::String(magnet));
                }

                // Special handling for size formatting if it's raw bytes
                if let Some(size_str) = mapped_item.get("size").and_then(|v| v.as_str())
                    && let Ok(bytes) = size_str.parse::<u64>()
                {
                    mapped_item.insert(
                        "size".to_string(),
                        serde_json::Value::String(format_bytes_simple(bytes)),
                    );
                }

                if !mapped_item.is_empty() {
                    results.push(serde_json::Value::Object(mapped_item));
                }
            }
        }
    } else {
        let document = scraper::Html::parse_document(&resp);
        let list_selector_str = p_conf
            .get("list")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No list selector"))?;
        let list_selector =
            scraper::Selector::parse(list_selector_str).map_err(|e| anyhow::anyhow!("{:?}", e))?;

        for element in document.select(&list_selector) {
            let mut item = serde_json::Map::new();
            let element_text = element.text().collect::<String>();

            if let Some(res_obj) = res_conf.as_object() {
                for (key, val) in res_obj {
                    let mut found_val = String::new();

                    match val {
                        serde_json::Value::String(s) if s.starts_with('/') && s.ends_with('/') => {
                            let re_str = &s[1..s.len() - 1];
                            if let Ok(re) = regex::Regex::new(re_str)
                                && let Some(caps) = re.captures(&element_text)
                            {
                                found_val =
                                    caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                            }
                        }
                        _ => {
                            let (selector_str, attr) = match val {
                                serde_json::Value::String(s) => (s.as_str(), None),
                                serde_json::Value::Array(arr) => (
                                    arr[0].as_str().unwrap_or(""),
                                    arr.get(1).and_then(|v| v.as_str()),
                                ),
                                _ => ("", None),
                            };

                            if let Ok(sel) = scraper::Selector::parse(selector_str)
                                && let Some(found) = element.select(&sel).next()
                            {
                                found_val = if let Some(a) = attr {
                                    if let Some(attr_name) = a.strip_prefix('@') {
                                        found.value().attr(attr_name).unwrap_or("").to_string()
                                    } else {
                                        found.text().collect::<String>()
                                    }
                                } else {
                                    found.text().collect::<String>()
                                };
                            }
                        }
                    }
                    item.insert(
                        key.clone(),
                        serde_json::Value::String(found_val.trim().to_string()),
                    );
                }
            }
            if !item.is_empty() {
                results.push(serde_json::Value::Object(item));
            }
        }
    }

    // Item follow-up logic
    let item_provider = format!("{}/item", provider);
    if let Some(item_conf) = config.get(&item_provider) {
        let mut follow_up_futures = vec![];
        for res in results.iter_mut() {
            if res.get("magnet").is_none()
                && let Some(item_url_path) = res.get("url").and_then(|v| v.as_str())
            {
                let item_conf = item_conf.clone();
                let client = client.clone();
                let item_url_path = item_url_path.to_string();

                follow_up_futures.push(async move {
                    let item_url_tpl = item_conf.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    let item_url = item_url_tpl.replace("{{item}}", &item_url_path);

                    if let Ok(item_resp) = client.get(&item_url).send().await
                        && let Ok(item_html) = item_resp.text().await
                    {
                        let item_doc = scraper::Html::parse_document(&item_html);
                        if let Some(res_obj) = item_conf.get("result").and_then(|v| v.as_object()) {
                            let mut item_data = serde_json::Map::new();
                            for (key, val) in res_obj {
                                let (selector_str, attr) = match val {
                                    serde_json::Value::String(s) => (s.as_str(), None),
                                    serde_json::Value::Array(arr) => (
                                        arr[0].as_str().unwrap_or(""),
                                        arr.get(1).and_then(|v| v.as_str()),
                                    ),
                                    _ => ("", None),
                                };
                                if let Ok(sel) = scraper::Selector::parse(selector_str)
                                    && let Some(found) = item_doc.select(&sel).next()
                                {
                                    let text = if let Some(a) = attr {
                                        if let Some(attr_name) = a.strip_prefix('@') {
                                            found.value().attr(attr_name).unwrap_or("").to_string()
                                        } else {
                                            found.text().collect::<String>()
                                        }
                                    } else {
                                        found.text().collect::<String>()
                                    };
                                    item_data.insert(
                                        key.clone(),
                                        serde_json::Value::String(text.trim().to_string()),
                                    );
                                }
                            }
                            return Some(item_data);
                        }
                    }
                    None
                });
            }
        }

        if !follow_up_futures.is_empty() {
            let follow_up_results = futures::future::join_all(follow_up_futures).await;
            let mut i = 0;
            for res in results.iter_mut() {
                if res.get("magnet").is_none() && res.get("url").is_some() {
                    if let Some(Some(item_data)) = follow_up_results.get(i) {
                        for (k, v) in item_data {
                            res.as_object_mut().unwrap().insert(k.clone(), v.clone());
                        }
                    }
                    i += 1;
                }
            }
        }
    }

    Ok(results)
}

fn format_bytes_simple(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let k = 1024.0;
    let sizes = ["B", "KB", "MB", "GB", "TB"];
    let i = (bytes as f64).log(k).floor() as usize;
    format!("{:.2} {}", bytes as f64 / k.powi(i as i32), sizes[i])
}

async fn api_search_providers(
    State((state, _)): State<(Arc<AppState>, tokio::sync::broadcast::Sender<()>)>,
) -> Json<serde_json::Value> {
    let scraper_url = state.engine.get_config().await.scraper_url;
    let config = load_scraper_config(&scraper_url).await;
    let json: serde_json::Value =
        serde_json::from_str(&config).unwrap_or_else(|_| serde_json::json!({}));

    let mut providers = serde_json::Map::new();
    if let Some(obj) = json.as_object() {
        for (k, v) in obj {
            if !k.ends_with("/item") {
                providers.insert(k.clone(), v.clone());
            }
        }
    }
    Json(serde_json::Value::Object(providers))
}

async fn static_handler(uri: axum::http::Uri) -> impl IntoResponse {
    let mut path = uri.path().trim_start_matches('/').to_string();

    if path.is_empty() || path == "/" {
        path = "index.html".to_string();
    }

    match Assets::get(&path) {
        Some(content) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(axum::body::Body::from(content.data))
                .unwrap()
        }
        None => {
            // If the path doesn't exist, try index.html for SPA routing fallback
            if path != "index.html"
                && let Some(content) = Assets::get("index.html")
            {
                return Response::builder()
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(axum::body::Body::from(content.data))
                    .unwrap();
            }
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(axum::body::Body::from("404 Not Found"))
                .unwrap()
        }
    }
}
