use std::num::NonZeroU32;

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
