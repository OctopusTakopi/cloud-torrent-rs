use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct Torrent {
    pub info_hash: String,
    pub name: String,
    pub magnet: String,
    pub loaded: bool,
    pub downloaded: i64,
    pub uploaded: i64,
    pub size: i64,
    pub percent: f32,
    pub status: String,
    pub download_rate: f32,
    pub upload_rate: f32,
    pub is_queueing: bool,
    pub is_seeding: bool,
    pub started: bool,
    pub added_at: String,
    pub peers_connected: u32,
    pub peers_total: u32,
    pub peers_half_open: u32,
    pub peers_pending: u32,
    pub seed_ratio: f32,
    #[serde(default)]
    pub added_at_ts: i64,
    pub files: Vec<serde_json::Value>,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct DhtStats {
    pub nodes4: usize,
    pub nodes6: usize,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct SystemStats {
    pub cpu: f32,
    pub mem_used_percent: f64,
    pub disk_used_percent: f64,
    pub disk_free: u64,
    pub app_memory: u64,
    pub active_tasks: u32,
    pub dht: DhtStats,
    #[serde(default)]
    pub version: String,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct ConnStat {
    pub bytes_written_data: u64,
    pub bytes_read_useful_data: u64,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct Stats {
    pub system: SystemStats,
    pub conn_stat: ConnStat,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct GlobalState {
    pub use_queue: bool,
    #[serde(rename = "LatestRSSGuid")]
    pub latest_rss_guid: String,
    #[serde(default)]
    pub rss_last_error: String,
    pub torrents: Vec<Torrent>,
    pub users: HashMap<String, serde_json::Value>,
    pub stats: Stats,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Default)]
#[serde(rename_all = "PascalCase")]
pub struct RssItem {
    pub id: String,
    pub title: String,
    pub link: String,
    pub load_url: String,
    pub source_title: String,
    pub source_url: String,
    pub published: String,
    #[serde(default)]
    pub published_ts: i64,
    #[serde(default)]
    pub is_new: bool,
    #[serde(default)]
    pub loaded: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Default)]
#[serde(rename_all = "PascalCase")]
pub struct RssSnapshot {
    pub items: Vec<RssItem>,
    #[serde(default)]
    pub latest_guid: String,
    #[serde(default)]
    pub last_updated: i64,
    #[serde(default)]
    pub last_error: String,
    #[serde(default)]
    pub feed_count: usize,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct Config {
    pub auto_start: bool,
    pub engine_debug: bool,
    pub mute_engine_log: bool,
    pub obfs_preferred: bool,
    pub obfs_require_preferred: bool,
    pub disable_trackers: bool,
    #[serde(alias = "DisableIPv6")]
    pub disable_ipv6: bool,
    pub no_default_port_forwarding: bool,
    #[serde(alias = "DisableUTP")]
    pub disable_utp: bool,
    pub download_directory: String,
    pub watch_directory: String,
    pub cache_directory: String,
    pub trash_directory: String,
    pub enable_upload: bool,
    pub enable_seeding: bool,
    pub incoming_port: i32,
    pub done_cmd: String,
    pub seed_ratio: f32,
    pub seed_time: String,
    pub upload_rate: String,
    pub download_rate: String,
    pub max_active_torrents: i32,
    pub max_concurrent_task: i32,
    pub always_add_trackers: bool,
    pub tracker_list: String,
    #[serde(alias = "scraper_url", alias = "scraperurl", alias = "ScraperURL")]
    pub scraper_url: String,
    #[serde(alias = "rss_url", alias = "rssurl", alias = "RSSURL")]
    pub rss_url: String,
    pub allow_runtime_configure: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_start: true,
            engine_debug: false,
            mute_engine_log: false,
            obfs_preferred: false,
            obfs_require_preferred: false,
            disable_trackers: false,
            disable_ipv6: false,
            no_default_port_forwarding: false,
            disable_utp: false,
            download_directory: "downloads".to_string(),
            watch_directory: "torrents".to_string(),
            cache_directory: "cache".to_string(),
            trash_directory: "trash".to_string(),
            enable_upload: true,
            enable_seeding: true,
            incoming_port: 50007,
            done_cmd: "".to_string(),
            seed_ratio: 0.0,
            seed_time: "0s".to_string(),
            upload_rate: "".to_string(),
            download_rate: "".to_string(),
            max_active_torrents: 0,
            max_concurrent_task: 0,
            always_add_trackers: false,
            tracker_list: "remote:https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt".to_string(),
            scraper_url: "https://raw.githubusercontent.com/OctopusTakopi/cloud-torrent-rs/master/scraper-config.json".to_string(),
            rss_url: "".to_string(),
            allow_runtime_configure: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub struct SearchResult {
    pub name: String,
    pub magnet: String,
    pub size: String,
    pub seeds: String,
    #[serde(default)]
    pub peers: String,
}
