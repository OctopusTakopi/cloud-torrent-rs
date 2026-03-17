use std::num::NonZeroU32;
use std::path::Path;

pub const DISK_SPACE_RESERVE_BYTES: u64 = 64 * 1024 * 1024;

pub fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

pub fn parse_rate(rate: &str) -> Option<u32> {
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

pub fn format_ago(timestamp: i64) -> String {
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

pub fn apply_ratelimits(session: &librqbit::Session, config: &cloud_torrent_common::Config) {
    let down_bps = parse_rate(&config.download_rate).and_then(NonZeroU32::new);
    let up_bps = if config.enable_upload {
        parse_rate(&config.upload_rate).and_then(NonZeroU32::new)
    } else {
        NonZeroU32::new(1)
    };

    session.ratelimits.set_download_bps(down_bps);
    session.ratelimits.set_upload_bps(up_bps);
}

pub fn available_space_for_path(path: &Path) -> u64 {
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    };
    let abs_path = std::fs::canonicalize(&abs_path).unwrap_or(abs_path);

    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut best_match: Option<&sysinfo::Disk> = None;
    for disk in &disks {
        if abs_path.starts_with(disk.mount_point()) {
            match best_match {
                Some(current)
                    if disk.mount_point().as_os_str().len()
                        > current.mount_point().as_os_str().len() =>
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
            .find(|disk| disk.mount_point() == std::path::Path::new("/"));
    }

    best_match.map(|disk| disk.available_space()).unwrap_or(0)
}

pub fn remaining_download_bytes(total_bytes: u64, progress_bytes: u64) -> Option<u64> {
    if total_bytes == 0 || progress_bytes >= total_bytes {
        None
    } else {
        Some(total_bytes - progress_bytes)
    }
}

pub fn format_storage_error(required: u64, free: u64) -> String {
    format!(
        "Need {} free for this download, but only {} is available in the download directory.",
        format_bytes_for_error(required),
        format_bytes_for_error(free)
    )
}

fn format_bytes_for_error(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", units[unit])
}
