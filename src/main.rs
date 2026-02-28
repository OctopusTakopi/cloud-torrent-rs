use anyhow::Result;
use clap::Parser;
use cloud_torrent_common::Config;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod engine;
mod server;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Listening address
    #[arg(short, long, default_value = "0.0.0.0:3000", env = "LISTEN")]
    listen: String,

    /// Title of this instance
    #[arg(short, long, default_value = "Cloud Torrent-rs", env = "TITLE")]
    title: String,

    /// Download directory
    #[arg(short, long, default_value = "downloads", env = "DOWNLOAD_DIR")]
    download_dir: String,

    /// Optional basic auth in form 'user:password'
    #[arg(long, env = "AUTH")]
    auth: Option<String>,

    /// TLS Certificate file path
    #[arg(short = 'r', long, env = "CERT_PATH")]
    cert_path: Option<String>,

    /// TLS Key file path
    #[arg(short = 'k', long, env = "KEY_PATH")]
    key_path: Option<String>,

    /// Unix domain socket file permission (e.g. 0666), only used when listen is a unix socket path
    #[arg(short = 'u', long, env = "UNIX_PERM", default_value = "0666")]
    unix_perm: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "cloud_torrent=debug,tower_http=debug,axum::rejection=trace".into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();

    let mut config = if std::path::Path::new("cloud-torrent.yaml").exists() {
        tracing::info!("Loading config from cloud-torrent.yaml");
        let content = std::fs::read_to_string("cloud-torrent.yaml")?;
        serde_yaml::from_str(&content)?
    } else {
        tracing::info!("Creating default cloud-torrent.yaml");
        let default_config = Config::default();
        let content = serde_yaml::to_string(&default_config)?;
        std::fs::write("cloud-torrent.yaml", content)?;
        default_config
    };

    config.download_directory = args.download_dir;
    let download_path = std::path::Path::new(&config.download_directory);
    config.cache_directory = download_path.join(".cache").to_string_lossy().into_owned();
    config.trash_directory = download_path.join(".trash").to_string_lossy().into_owned();

    tracing::info!(
        "Starting {} v{} on {}",
        args.title,
        env!("CARGO_PKG_VERSION"),
        args.listen
    );

    // Initialize engine
    let (engine, changed_rx) = engine::Engine::new(config).await?;

    // Detect unix socket vs TCP
    let is_unix = args.listen.starts_with("unix:");
    let unix_path = if is_unix {
        Some(args.listen.trim_start_matches("unix:").to_string())
    } else {
        None
    };

    let tcp_addr: Option<std::net::SocketAddr> = if is_unix {
        None
    } else {
        Some(args.listen.parse()?)
    };

    // Start server
    server::run(
        tcp_addr,
        unix_path,
        args.unix_perm,
        args.title,
        engine,
        changed_rx,
        args.auth,
        args.cert_path,
        args.key_path,
    )
    .await?;

    Ok(())
}
