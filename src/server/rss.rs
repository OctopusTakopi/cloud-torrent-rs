use crate::engine::Engine;
use crate::engine::build_http_client;
use crate::engine::types::{PersistedRssFeed, PersistedRssState};
use cloud_torrent_common::{RssItem, RssSnapshot};
use futures::future::join_all;
use quick_xml::Reader;
use quick_xml::events::Event;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use url::Url;

const RSS_MAX_ITEMS_PER_FEED: usize = 100;
const RSS_MAX_ITEMS_TOTAL: usize = 300;
const RSS_MAX_SEEN_ITEMS: usize = 5000;
pub const RSS_REFRESH_INTERVAL_SECS: u64 = 300;

static MAGNET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"magnet:\?[^\s"'<>]+"#).expect("valid magnet regex"));

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s"'<>]+"#).expect("valid url regex"));

static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").expect("valid html strip regex"));

#[derive(Clone)]
pub struct RssService {
    state: std::sync::Arc<tokio::sync::RwLock<PersistedRssState>>,
    refresh_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

pub struct RssRefreshOutcome {
    pub snapshot: RssSnapshot,
    pub changed: bool,
}

#[derive(Default)]
struct ParsedFeed {
    title: String,
    items: Vec<ParsedFeedItem>,
}

#[derive(Default)]
struct ParsedFeedItem {
    title: String,
    guid: String,
    link: String,
    load_url: String,
    published: String,
    published_ts: i64,
    info_hash: Option<String>,
}

#[derive(Default)]
struct PartialFeedItem {
    title: String,
    guid: String,
    published_raw: String,
    links: Vec<LinkCandidate>,
    blobs: Vec<String>,
}

#[derive(Default)]
struct LinkCandidate {
    url: String,
    rel: String,
    mime: String,
}

#[derive(Clone, Copy)]
enum CaptureKind {
    FeedTitle,
    ItemTitle,
    Guid,
    Published,
    Blob,
    LinkText,
}

struct CaptureState {
    kind: CaptureKind,
    depth: usize,
    text: String,
}

impl RssService {
    pub fn new(initial: PersistedRssState) -> Self {
        Self {
            state: std::sync::Arc::new(tokio::sync::RwLock::new(initial)),
            refresh_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn snapshot(&self) -> RssSnapshot {
        let state = self.state.read().await;
        snapshot_from_state(&state, state.feeds.len())
    }

    pub async fn mark_item_loaded(
        &self,
        engine: &Engine,
        item_id: &str,
    ) -> anyhow::Result<RssSnapshot> {
        let mut state = self.state.write().await;
        let now = now_ts();

        state.seen_items.insert(item_id.to_string(), now);
        state.loaded_items.insert(item_id.to_string(), now);

        for feed in state.feeds.values_mut() {
            for item in &mut feed.items {
                if item.id == item_id {
                    item.loaded = true;
                    item.is_new = false;
                }
            }
        }

        prune_tracked_items(&mut state);
        let snapshot = snapshot_from_state(&state, state.feeds.len());
        engine.storage.save_rss_state(&state)?;
        Ok(snapshot)
    }

    pub async fn refresh(&self, engine: &Engine) -> RssRefreshOutcome {
        let _guard = self.refresh_lock.lock().await;

        let before = self.state.read().await.clone();
        let before_snapshot = snapshot_from_state(&before, before.feeds.len());

        let urls = configured_rss_urls(&engine.get_config().await.rss_url);
        if urls.is_empty() {
            let mut cleared = before;
            cleared.feeds.clear();
            cleared.last_error.clear();
            cleared.last_updated = 0;
            cleared.latest_guid.clear();
            cleared.seen_items.clear();
            cleared.loaded_items.clear();
            let snapshot = snapshot_from_state(&cleared, 0);
            {
                let mut state = self.state.write().await;
                *state = cleared.clone();
            }
            if let Err(e) = engine.storage.save_rss_state(&cleared) {
                tracing::error!("Failed to persist RSS state: {}", e);
            }
            return RssRefreshOutcome {
                changed: snapshot != before_snapshot,
                snapshot,
            };
        }

        let mut working = before.clone();
        let now = now_ts();
        let mut existing_hashes = existing_info_hashes(engine).await;
        let mut any_success = false;
        let mut fetch_errors = Vec::new();
        let configured: HashSet<String> = urls.iter().cloned().collect();

        let fetched = join_all(urls.iter().cloned().map(|url| async move {
            let result = fetch_feed(&url).await;
            (url, result)
        }))
        .await;

        for (url, result) in fetched {
            match result {
                Ok(parsed) => {
                    any_success = true;
                    let mut items = Vec::new();
                    for parsed_item in parsed.items {
                        let item_id = item_key(&url, &parsed_item);
                        let seen_before = working.seen_items.contains_key(&item_id);
                        let mut loaded = working.loaded_items.contains_key(&item_id)
                            || parsed_item
                                .info_hash
                                .as_ref()
                                .map(|hash| existing_hashes.contains(hash))
                                .unwrap_or(false);
                        let is_new = working.initialized && !seen_before;
                        let mut should_mark_seen = seen_before || !working.initialized || loaded;

                        if is_new && !loaded {
                            match engine.add_magnet(&parsed_item.load_url).await {
                                Ok(()) => {
                                    loaded = true;
                                    should_mark_seen = true;
                                    working.loaded_items.insert(item_id.clone(), now);
                                    if let Some(hash) = &parsed_item.info_hash {
                                        existing_hashes.insert(hash.clone());
                                    }
                                }
                                Err(e) => {
                                    fetch_errors.push(format!(
                                        "{}: failed to load '{}' ({})",
                                        url, parsed_item.title, e
                                    ));
                                    tracing::error!(
                                        "Failed to auto-load RSS item '{}' from {}: {}",
                                        parsed_item.title,
                                        &url,
                                        e
                                    );
                                }
                            }
                        }

                        if loaded {
                            working.loaded_items.insert(item_id.clone(), now);
                        }
                        if should_mark_seen {
                            working.seen_items.insert(item_id.clone(), now);
                        }

                        items.push(RssItem {
                            id: item_id,
                            title: parsed_item.title,
                            link: parsed_item.link,
                            load_url: parsed_item.load_url,
                            source_title: if parsed.title.is_empty() {
                                url.clone()
                            } else {
                                parsed.title.clone()
                            },
                            source_url: url.clone(),
                            published: parsed_item.published,
                            published_ts: parsed_item.published_ts,
                            is_new,
                            loaded,
                        });
                    }

                    items.sort_by(sort_items_desc);
                    items.truncate(RSS_MAX_ITEMS_PER_FEED);

                    working.feeds.insert(
                        url.clone(),
                        PersistedRssFeed {
                            title: parsed.title,
                            items,
                            last_updated: now,
                            last_error: String::new(),
                        },
                    );
                }
                Err(e) => {
                    tracing::warn!("RSS refresh failed for {}: {}", &url, e);
                    fetch_errors.push(format!("{}: {}", url, e));
                    let feed = working.feeds.entry(url.clone()).or_default();
                    feed.last_error = e.to_string();
                }
            }
        }

        working.feeds.retain(|url, _| configured.contains(url));

        if any_success {
            working.initialized = true;
            working.last_updated = now;
        }

        working.last_error = fetch_errors.join("\n");
        let preview = snapshot_from_state(&working, urls.len());
        working.latest_guid = preview
            .items
            .first()
            .map(|item| item.id.clone())
            .unwrap_or_default();

        prune_tracked_items(&mut working);

        {
            let mut state = self.state.write().await;
            *state = working.clone();
        }
        if let Err(e) = engine.storage.save_rss_state(&working) {
            tracing::error!("Failed to persist RSS state: {}", e);
        }

        let snapshot = snapshot_from_state(&working, urls.len());
        RssRefreshOutcome {
            changed: snapshot != before_snapshot,
            snapshot,
        }
    }
}

fn configured_rss_urls(config: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut urls = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !(line.starts_with("http://") || line.starts_with("https://")) {
            continue;
        }
        if seen.insert(line.to_string()) {
            urls.push(line.to_string());
        }
    }
    urls
}

fn snapshot_from_state(state: &PersistedRssState, feed_count: usize) -> RssSnapshot {
    let mut items = Vec::new();
    let mut seen_ids = HashSet::new();
    for feed in state.feeds.values() {
        for item in &feed.items {
            if seen_ids.insert(item.id.clone()) {
                items.push(item.clone());
            }
        }
    }

    items.sort_by(sort_items_desc);
    items.truncate(RSS_MAX_ITEMS_TOTAL);

    RssSnapshot {
        items,
        latest_guid: state.latest_guid.clone(),
        last_updated: state.last_updated,
        last_error: state.last_error.clone(),
        feed_count,
    }
}

fn sort_items_desc(left: &RssItem, right: &RssItem) -> std::cmp::Ordering {
    right
        .published_ts
        .cmp(&left.published_ts)
        .then_with(|| left.title.cmp(&right.title))
        .then_with(|| left.id.cmp(&right.id))
}

async fn existing_info_hashes(engine: &Engine) -> HashSet<String> {
    engine
        .get_torrents()
        .await
        .into_iter()
        .map(|torrent| torrent.info_hash.to_lowercase())
        .collect()
}

async fn fetch_feed(url: &str) -> anyhow::Result<ParsedFeed> {
    let response = build_http_client()
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            "application/rss+xml, application/atom+xml, application/xml, text/xml;q=0.9, */*;q=0.8",
        )
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!("HTTP {}", status));
    }
    let body = response.text().await?;
    parse_feed_document(url, &body)
}

fn parse_feed_document(source_url: &str, xml: &str) -> anyhow::Result<ParsedFeed> {
    let base_url = Url::parse(source_url).ok();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut depth = 0usize;
    let mut feed_title = String::new();
    let mut capture: Option<CaptureState> = None;
    let mut current_item: Option<PartialFeedItem> = None;
    let mut items = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(start)) => {
                depth += 1;
                let local = start.local_name();
                let name = local_name(local.as_ref()).to_string();

                if let Some(item) = current_item.as_mut()
                    && matches!(name.as_str(), "a" | "link" | "enclosure" | "content")
                {
                    collect_link_candidate(start.attributes(), &base_url, item, &name);
                }

                if matches!(name.as_str(), "item" | "entry") {
                    current_item = Some(PartialFeedItem::default());
                } else if let Some(item) = current_item.as_mut() {
                    if capture.is_none() {
                        match name.as_str() {
                            "title" if item.title.is_empty() => {
                                capture = Some(CaptureState {
                                    kind: CaptureKind::ItemTitle,
                                    depth,
                                    text: String::new(),
                                });
                            }
                            "guid" | "id" if item.guid.is_empty() => {
                                capture = Some(CaptureState {
                                    kind: CaptureKind::Guid,
                                    depth,
                                    text: String::new(),
                                });
                            }
                            "pubDate" | "published" | "updated" | "date"
                                if item.published_raw.is_empty() =>
                            {
                                capture = Some(CaptureState {
                                    kind: CaptureKind::Published,
                                    depth,
                                    text: String::new(),
                                });
                            }
                            "description" | "summary" | "content" | "encoded" => {
                                capture = Some(CaptureState {
                                    kind: CaptureKind::Blob,
                                    depth,
                                    text: String::new(),
                                });
                            }
                            "link" => {
                                let has_href = start
                                    .attributes()
                                    .with_checks(false)
                                    .flatten()
                                    .any(|attr| local_name(attr.key.as_ref()) == "href");
                                if !has_href {
                                    capture = Some(CaptureState {
                                        kind: CaptureKind::LinkText,
                                        depth,
                                        text: String::new(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                } else if feed_title.is_empty() && capture.is_none() && name == "title" {
                    capture = Some(CaptureState {
                        kind: CaptureKind::FeedTitle,
                        depth,
                        text: String::new(),
                    });
                }
            }
            Ok(Event::Empty(empty)) => {
                let local = empty.local_name();
                let name = local_name(local.as_ref()).to_string();
                if let Some(item) = current_item.as_mut()
                    && matches!(name.as_str(), "link" | "enclosure" | "content" | "a")
                {
                    collect_link_candidate(empty.attributes(), &base_url, item, &name);
                }
            }
            Ok(Event::Text(text)) => {
                if let Some(active) = capture.as_mut() {
                    active
                        .text
                        .push_str(&decode_entities(&String::from_utf8_lossy(text.as_ref())));
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(active) = capture.as_mut() {
                    active
                        .text
                        .push_str(&String::from_utf8_lossy(text.as_ref()));
                }
            }
            Ok(Event::End(end)) => {
                if capture.as_ref().is_some_and(|active| active.depth == depth) {
                    let finished = capture.take().expect("capture exists");
                    finalize_capture(finished, &mut feed_title, current_item.as_mut(), &base_url);
                }

                let local = end.local_name();
                let name = local_name(local.as_ref()).to_string();
                if matches!(name.as_str(), "item" | "entry")
                    && let Some(item) = current_item.take()
                    && let Some(parsed) = finalize_item(item)
                {
                    items.push(parsed);
                }

                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    Ok(ParsedFeed {
        title: clean_text(&feed_title),
        items,
    })
}

fn collect_link_candidate<'a>(
    mut attrs: quick_xml::events::attributes::Attributes<'a>,
    base_url: &Option<Url>,
    item: &mut PartialFeedItem,
    tag_name: &str,
) {
    let mut url = None;
    let mut rel = String::new();
    let mut mime = String::new();

    for attr in attrs.with_checks(false).flatten() {
        let value = decode_entities(&String::from_utf8_lossy(attr.value.as_ref()));
        match local_name(attr.key.as_ref()) {
            "href" | "url" | "src" => url = Some(resolve_url(value.trim(), base_url)),
            "rel" => rel = value.trim().to_ascii_lowercase(),
            "type" => mime = value.trim().to_ascii_lowercase(),
            _ => {}
        }
    }

    if let Some(url) = url.filter(|value| !value.is_empty()) {
        item.links.push(LinkCandidate { url, rel, mime });
    } else if tag_name == "content" {
        // Atom content may point to downloadable content through nested text later.
    }
}

fn finalize_capture(
    capture: CaptureState,
    feed_title: &mut String,
    current_item: Option<&mut PartialFeedItem>,
    base_url: &Option<Url>,
) {
    let text = capture.text.trim();
    if text.is_empty() {
        return;
    }

    match capture.kind {
        CaptureKind::FeedTitle => {
            if feed_title.is_empty() {
                *feed_title = clean_text(text);
            }
        }
        CaptureKind::ItemTitle => {
            if let Some(item) = current_item
                && item.title.is_empty()
            {
                item.title = clean_text(text);
            }
        }
        CaptureKind::Guid => {
            if let Some(item) = current_item
                && item.guid.is_empty()
            {
                item.guid = clean_text(text);
            }
        }
        CaptureKind::Published => {
            if let Some(item) = current_item
                && item.published_raw.is_empty()
            {
                item.published_raw = clean_text(text);
            }
        }
        CaptureKind::Blob => {
            if let Some(item) = current_item {
                item.blobs.push(resolve_blob_text(text, base_url));
            }
        }
        CaptureKind::LinkText => {
            if let Some(item) = current_item {
                let url = resolve_url(text, base_url);
                if !url.is_empty() {
                    item.links.push(LinkCandidate {
                        url,
                        rel: String::new(),
                        mime: String::new(),
                    });
                }
            }
        }
    }
}

fn finalize_item(item: PartialFeedItem) -> Option<ParsedFeedItem> {
    let title = if item.title.is_empty() {
        "Untitled RSS item".to_string()
    } else {
        item.title
    };

    let mut link = String::new();
    let mut load_url = String::new();

    for candidate in &item.links {
        if load_url.is_empty() {
            if candidate.url.starts_with("magnet:") {
                load_url = normalize_magnet(&candidate.url);
            } else if is_torrent_candidate(candidate) {
                load_url = candidate.url.clone();
            }
        }

        if link.is_empty() && candidate.url.starts_with("http") && !is_likely_load_link(candidate) {
            link = candidate.url.clone();
        }
    }

    if load_url.is_empty() {
        for blob in &item.blobs {
            if let Some(magnet) = extract_magnet(blob) {
                load_url = magnet;
                break;
            }
        }
    }

    if load_url.is_empty() {
        for blob in &item.blobs {
            if let Some(url) = extract_torrent_url(blob) {
                load_url = url;
                break;
            }
        }
    }

    if load_url.is_empty() {
        return None;
    }

    if link.is_empty() && load_url.starts_with("http") {
        link = load_url.clone();
    }

    let published_ts = parse_published_ts(&item.published_raw);
    let published = format_published(&item.published_raw, published_ts);
    let info_hash = magnet_info_hash(&load_url);

    Some(ParsedFeedItem {
        title,
        guid: item.guid,
        link,
        load_url,
        published,
        published_ts,
        info_hash,
    })
}

fn item_key(source_url: &str, item: &ParsedFeedItem) -> String {
    if let Some(hash) = &item.info_hash {
        return format!("ih:{}", hash);
    }
    if !item.guid.is_empty() {
        return format!(
            "guid:{:x}",
            md5::compute(format!("{}|{}", source_url, item.guid.trim()))
        );
    }
    if !item.link.is_empty() {
        return format!(
            "link:{:x}",
            md5::compute(format!("{}|{}", source_url, item.link.trim()))
        );
    }
    format!(
        "item:{:x}",
        md5::compute(format!(
            "{}|{}|{}|{}",
            source_url, item.title, item.published_ts, item.load_url
        ))
    )
}

fn prune_tracked_items(state: &mut PersistedRssState) {
    let mut keep_seen = HashMap::new();
    let mut keep_loaded = HashMap::new();
    for feed in state.feeds.values() {
        for item in &feed.items {
            if let Some(ts) = state.seen_items.get(&item.id) {
                keep_seen.insert(item.id.clone(), *ts);
            }
            if let Some(ts) = state.loaded_items.get(&item.id) {
                keep_loaded.insert(item.id.clone(), *ts);
            }
        }
    }

    let mut sorted_seen = state
        .seen_items
        .iter()
        .map(|(id, ts)| (id.clone(), *ts))
        .collect::<Vec<_>>();
    sorted_seen.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));

    for (id, ts) in sorted_seen {
        if keep_seen.len() >= RSS_MAX_SEEN_ITEMS {
            break;
        }
        keep_seen.entry(id).or_insert(ts);
    }

    let mut sorted_loaded = state
        .loaded_items
        .iter()
        .map(|(id, ts)| (id.clone(), *ts))
        .collect::<Vec<_>>();
    sorted_loaded.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));

    for (id, ts) in sorted_loaded {
        if keep_loaded.len() >= RSS_MAX_SEEN_ITEMS {
            break;
        }
        keep_loaded.entry(id).or_insert(ts);
    }

    state.seen_items = keep_seen;
    state.loaded_items = keep_loaded;
}

fn local_name(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|name| name.rsplit(':').next())
        .unwrap_or("")
}

fn clean_text(text: &str) -> String {
    let stripped = TAG_RE.replace_all(text, " ");
    decode_entities(stripped.trim())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_blob_text(text: &str, base_url: &Option<Url>) -> String {
    let decoded = decode_entities(text);
    let mut resolved = decoded.to_string();
    if let Some(base) = base_url {
        resolved = resolved.replace(
            "href=\"/",
            &format!("href=\"{}/", base.origin().ascii_serialization()),
        );
        resolved = resolved.replace(
            "src=\"/",
            &format!("src=\"{}/", base.origin().ascii_serialization()),
        );
    }
    resolved
}

fn resolve_url(raw: &str, base_url: &Option<Url>) -> String {
    let trimmed = decode_entities(raw)
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '<' | '>'))
        .trim_end_matches(['.', ',', ';', ')', ']'])
        .to_string();

    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("magnet:") {
        return normalize_magnet(&trimmed);
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed;
    }
    if let Some(base) = base_url
        && let Ok(url) = base.join(&trimmed)
    {
        return url.to_string();
    }
    trimmed
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
}

fn normalize_magnet(text: &str) -> String {
    decode_entities(text)
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '<' | '>'))
        .trim_end_matches(['.', ',', ';', ')', ']'])
        .to_string()
}

fn extract_magnet(text: &str) -> Option<String> {
    MAGNET_RE
        .find(&decode_entities(text))
        .map(|matched| normalize_magnet(matched.as_str()))
}

fn extract_torrent_url(text: &str) -> Option<String> {
    let decoded = decode_entities(text);
    URL_RE.find_iter(&decoded).find_map(|matched| {
        let url = resolve_url(matched.as_str(), &None);
        if url.to_ascii_lowercase().contains(".torrent") {
            Some(url)
        } else {
            None
        }
    })
}

fn is_torrent_candidate(candidate: &LinkCandidate) -> bool {
    candidate.url.starts_with("http")
        && (candidate.url.to_ascii_lowercase().contains(".torrent")
            || candidate.mime.contains("bittorrent")
            || candidate.rel == "enclosure")
}

fn is_likely_load_link(candidate: &LinkCandidate) -> bool {
    candidate.url.starts_with("magnet:") || is_torrent_candidate(candidate)
}

fn magnet_info_hash(load_url: &str) -> Option<String> {
    if !load_url.starts_with("magnet:") {
        return None;
    }
    librqbit::Magnet::parse(load_url)
        .ok()
        .and_then(|magnet| magnet.as_id20().map(|hash| hash.as_string().to_lowercase()))
}

fn parse_published_ts(value: &str) -> i64 {
    let value = value.trim();
    if value.is_empty() {
        return 0;
    }

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return dt.timestamp();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(value) {
        return dt.timestamp();
    }

    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d",
        "%a, %d %b %Y %H:%M:%S %Z",
        "%a, %d %b %Y %H:%M:%S %z",
    ] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(value, fmt) {
            return chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc)
                .timestamp();
        }
        if let Ok(date) = chrono::NaiveDate::parse_from_str(value, fmt)
            && let Some(dt) = date.and_hms_opt(0, 0, 0)
        {
            return chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc)
                .timestamp();
        }
    }

    0
}

fn format_published(raw: &str, timestamp: i64) -> String {
    if timestamp > 0
        && let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
    {
        return dt.format("%Y-%m-%d %H:%M UTC").to_string();
    }
    clean_text(raw)
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rss_with_magnet_enclosure() {
        let xml = r#"
            <rss version="2.0">
              <channel>
                <title>Episodes</title>
                <item>
                  <title><![CDATA[Episode 1]]></title>
                  <guid>episode-1</guid>
                  <pubDate>Tue, 17 Mar 2026 10:00:00 GMT</pubDate>
                  <link>https://example.com/episode-1</link>
                  <enclosure url="magnet:?xt=urn:btih:0123456789ABCDEF0123456789ABCDEF01234567&amp;dn=Episode+1" type="application/x-bittorrent" />
                </item>
              </channel>
            </rss>
        "#;

        let feed = parse_feed_document("https://example.com/feed.xml", xml).unwrap();
        assert_eq!(feed.title, "Episodes");
        assert_eq!(feed.items.len(), 1);
        assert_eq!(feed.items[0].title, "Episode 1");
        assert_eq!(feed.items[0].link, "https://example.com/episode-1");
        assert!(
            feed.items[0]
                .load_url
                .starts_with("magnet:?xt=urn:btih:0123456789ABCDEF")
        );
    }

    #[test]
    fn parses_atom_with_magnet_in_content() {
        let xml = r#"
            <feed xmlns="http://www.w3.org/2005/Atom">
              <title>Nightlies</title>
              <entry>
                <title>Build 42</title>
                <id>tag:example.com,2026:42</id>
                <updated>2026-03-17T11:30:00Z</updated>
                <link rel="alternate" href="https://example.com/build-42" />
                <content type="html"><![CDATA[
                  <p><a href="magnet:?xt=urn:btih:89ABCDEF0123456789ABCDEF0123456789ABCDEF&amp;dn=Build+42">Download</a></p>
                ]]></content>
              </entry>
            </feed>
        "#;

        let feed = parse_feed_document("https://example.com/atom.xml", xml).unwrap();
        assert_eq!(feed.title, "Nightlies");
        assert_eq!(feed.items.len(), 1);
        assert_eq!(feed.items[0].title, "Build 42");
        assert_eq!(feed.items[0].link, "https://example.com/build-42");
        assert!(
            feed.items[0]
                .load_url
                .starts_with("magnet:?xt=urn:btih:89ABCDEF")
        );
    }
}
