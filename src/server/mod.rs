pub mod handlers;
pub mod middleware;
pub mod rss;
pub mod scraper;
pub mod state;
pub mod types;

use crate::engine::Engine;
use axum::{
    Router,
    routing::{get, post},
};
use core::net::SocketAddr;
use rust_embed::RustEmbed;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

use self::handlers::*;
use self::middleware::auth_middleware;
use self::rss::{RSS_REFRESH_INTERVAL_SECS, RssService};
use self::state::{AppState, SharedState};

#[derive(RustEmbed)]
#[folder = "frontend/dist"]
pub struct Assets;

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
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(a.as_bytes());
        format!("Basic {}", b64)
    });

    let mut sys = sysinfo::System::new_all();
    sys.refresh_all();

    let state = Arc::new(AppState {
        rss: Arc::new(RssService::new(
            engine
                .storage
                .load_rss_state()
                .unwrap_or_default()
                .unwrap_or_default(),
        )),
        engine,
        expected_auth,
        sys: tokio::sync::Mutex::new(sys),
    });

    let broadcast_tx_cloned = broadcast_tx.clone();
    tokio::spawn(async move {
        while changed_rx.recv().await.is_some() {
            let _ = broadcast_tx_cloned.send(());
        }
    });

    let state_for_rss = state.clone();
    let broadcast_tx_for_rss = broadcast_tx.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(RSS_REFRESH_INTERVAL_SECS));

        let initial = state_for_rss.rss.refresh(&state_for_rss.engine).await;
        if initial.changed {
            let _ = broadcast_tx_for_rss.send(());
        }

        loop {
            interval.tick().await;
            let outcome = state_for_rss.rss.refresh(&state_for_rss.engine).await;
            if outcome.changed {
                let _ = broadcast_tx_for_rss.send(());
            }
        }
    });

    let app: Router<()> = Router::<SharedState>::new()
        .route("/sync", get(sync_handler))
        .route("/sync/ws", get(sync_ws_handler))
        .route("/rss", get(api_rss))
        .route("/api/rss/load", post(api_rss_load))
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
        .with_state(SharedState {
            app_state: state.clone(),
            broadcast_tx: broadcast_tx.clone(),
        })
        .layer(CorsLayer::permissive())
        .layer(axum::middleware::from_fn_with_state(
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
        let _ = std::fs::remove_file(&sock_path);
        tracing::info!("Listening on unix:{}", sock_path);
        let listener = std::os::unix::net::UnixListener::bind(&sock_path)
            .map_err(|e| anyhow::anyhow!("Failed to bind unix socket {}: {}", sock_path, e))?;

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
