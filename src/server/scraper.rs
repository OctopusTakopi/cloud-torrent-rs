use crate::engine::build_http_client;
use crate::server::types::AppResult;

pub async fn load_scraper_config(
    remote_url: &str,
    empty_if_fail: bool,
) -> serde_json::Map<String, serde_json::Value> {
    // 1. Try local file first
    let local_path = "scraper-config.json";
    if std::path::Path::new(local_path).exists()
        && let Ok(content) = tokio::fs::read_to_string(local_path).await
        && let Ok(json) =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&content)
    {
        return json;
    }

    // 2. Fallback to remote
    match build_http_client().get(remote_url).send().await {
        Ok(resp) => {
            if let Ok(json) = resp
                .json::<serde_json::Map<String, serde_json::Value>>()
                .await
            {
                return json;
            }
        }
        Err(e) => {
            tracing::error!("Failed to fetch remote scraper config: {}", e);
        }
    }

    // 3. Fallback to embedded or empty
    if empty_if_fail {
        serde_json::Map::new()
    } else {
        tracing::warn!("Falling back to embedded scraper config");
        serde_json::from_str(include_str!("../../scraper-config.json")).unwrap_or_default()
    }
}

pub async fn search_scraper(
    query: &str,
    provider: Option<String>,
    config: &serde_json::Map<String, serde_json::Value>,
) -> AppResult<Vec<serde_json::Value>> {
    let client = build_http_client();
    let mut items: Vec<serde_json::Value> = vec![];

    let providers: Vec<String> = if let Some(p) = provider {
        if config.contains_key(&p) {
            vec![p]
        } else {
            vec![]
        }
    } else {
        config
            .keys()
            .filter(|k| !k.contains("/item"))
            .cloned()
            .collect()
    };

    let re_page = regex::Regex::new(r"\{\{page:(\d+)\}\}").unwrap();

    let mut futures = vec![];
    for p in providers {
        if let Some(p_conf) = config.get(&p) {
            let p_conf = p_conf.clone();
            let client = client.clone();
            let query = query.to_string();
            let p_name = p.clone();
            let re_page = re_page.clone();
            futures.push(async move {
                let url_tpl = p_conf.get("url").and_then(|v| v.as_str()).unwrap_or("");
                // Basic page support
                let mut url = url_tpl.replace("{{query}}", &urlencoding::encode(&query));
                url = re_page.replace_all(&url, "$1").to_string();

                let is_json = p_conf
                    .get("json")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let mut rb = client.get(&url);
                if let Some(headers_obj) = p_conf.get("headers").and_then(|v| v.as_object()) {
                    for (h_key, h_val) in headers_obj {
                        if let Some(h_val_str) = h_val.as_str() {
                            rb = rb.header(h_key, h_val_str);
                        }
                    }
                }

                if let Ok(resp) = rb.send().await {
                    if is_json {
                        if let Ok(json_val) = resp.json::<serde_json::Value>().await {
                            let mut results = vec![];
                            if let Some(items_arr) = json_val.as_array()
                                && let Some(res_obj) =
                                    p_conf.get("result").and_then(|v| v.as_object())
                            {
                                for item in items_arr {
                                    let mut item_data = serde_json::Map::new();
                                    for (key, mapping) in res_obj {
                                        if let Some(val_str) = mapping.as_str()
                                            && let Some(val) = item.get(val_str)
                                        {
                                            item_data.insert(key.clone(), val.clone());
                                        }
                                    }
                                    if !item_data.is_empty() {
                                        if !item_data.contains_key("magnet") {
                                            if let Some(ih) = item_data
                                                .get("infohash")
                                                .or_else(|| item_data.get("info_hash"))
                                                .and_then(|v| v.as_str())
                                            {
                                                item_data.insert(
                                                    "magnet".to_string(),
                                                    serde_json::json!(format!(
                                                        "magnet:?xt=urn:btih:{}",
                                                        ih
                                                    )),
                                                );
                                            } else {
                                                item_data.insert(
                                                    "magnet".to_string(),
                                                    serde_json::json!(""),
                                                );
                                            }
                                        }
                                        results.push(serde_json::Value::Object(item_data));
                                    }
                                }
                            }
                            for r in &mut results {
                                if let Some(obj) = r.as_object_mut() {
                                    obj.insert("provider".to_string(), serde_json::json!(p_name));
                                }
                            }
                            return results;
                        }
                    } else if let Ok(html) = resp.text().await {
                        let doc = scraper::Html::parse_document(&html);
                        let item_selector_str =
                            p_conf.get("list").and_then(|v| v.as_str()).unwrap_or("");
                        if let Ok(sel) = scraper::Selector::parse(item_selector_str) {
                            let mut results = vec![];
                            for element in doc.select(&sel) {
                                if let Some(res_obj) =
                                    p_conf.get("result").and_then(|v| v.as_object())
                                {
                                    let mut item_data = serde_json::Map::new();
                                    for (key, val) in res_obj {
                                        let (selector_str, attr_or_regex) = match val {
                                            serde_json::Value::String(s) => (s.as_str(), None),
                                            serde_json::Value::Array(arr) => (
                                                arr[0].as_str().unwrap_or(""),
                                                arr.get(1).and_then(|v| v.as_str()),
                                            ),
                                            _ => ("", None),
                                        };

                                        if selector_str.is_empty() {
                                            if let Some(r_str) = attr_or_regex
                                                && r_str.starts_with('/')
                                            {
                                                let row_text =
                                                    element.text().collect::<Vec<_>>().join(" ");
                                                if let Ok(re) =
                                                    regex::Regex::new(r_str.trim_matches('/'))
                                                    && let Some(caps) = re.captures(&row_text)
                                                {
                                                    let text = caps
                                                        .get(1)
                                                        .map_or("", |m| m.as_str())
                                                        .to_string();
                                                    item_data.insert(
                                                        key.clone(),
                                                        serde_json::json!(text),
                                                    );
                                                }
                                            }
                                            continue;
                                        }

                                        if let Ok(s) = scraper::Selector::parse(selector_str)
                                            && let Some(found) = element.select(&s).next()
                                        {
                                            let text = if let Some(a) = attr_or_regex {
                                                if let Some(stripped) = a.strip_prefix('@') {
                                                    found
                                                        .value()
                                                        .attr(stripped)
                                                        .unwrap_or("")
                                                        .to_string()
                                                } else if a.starts_with('/') {
                                                    let full_text =
                                                        found.text().collect::<Vec<_>>().join(" ");
                                                    let r_clean = a.trim_matches('/');
                                                    if let Ok(re) = regex::Regex::new(r_clean) {
                                                        re.captures(&full_text).map_or(
                                                            "".to_string(),
                                                            |caps| {
                                                                caps.get(1)
                                                                    .map_or("".to_string(), |m| {
                                                                        m.as_str().to_string()
                                                                    })
                                                            },
                                                        )
                                                    } else {
                                                        full_text
                                                    }
                                                } else {
                                                    found
                                                        .text()
                                                        .collect::<Vec<_>>()
                                                        .join(" ")
                                                        .trim()
                                                        .to_string()
                                                }
                                            } else {
                                                found
                                                    .text()
                                                    .collect::<Vec<_>>()
                                                    .join(" ")
                                                    .trim()
                                                    .to_string()
                                            };
                                            item_data.insert(key.clone(), serde_json::json!(text));
                                        }
                                    }
                                    if !item_data.is_empty() {
                                        if !item_data.contains_key("magnet") {
                                            if let Some(ih) = item_data
                                                .get("infohash")
                                                .or_else(|| item_data.get("info_hash"))
                                                .and_then(|v| v.as_str())
                                            {
                                                item_data.insert(
                                                    "magnet".to_string(),
                                                    serde_json::json!(format!(
                                                        "magnet:?xt=urn:btih:{}",
                                                        ih
                                                    )),
                                                );
                                            } else {
                                                item_data.insert(
                                                    "magnet".to_string(),
                                                    serde_json::json!(""),
                                                );
                                            }
                                        }
                                        if !item_data.contains_key("peers") {
                                            item_data.insert(
                                                "peers".to_string(),
                                                serde_json::json!("0"),
                                            );
                                        }
                                        if !item_data.contains_key("seeds") {
                                            item_data.insert(
                                                "seeds".to_string(),
                                                serde_json::json!("0"),
                                            );
                                        }
                                        results.push(serde_json::Value::Object(item_data));
                                    }
                                }
                            }
                            for r in &mut results {
                                if let Some(obj) = r.as_object_mut() {
                                    obj.insert("provider".to_string(), serde_json::json!(p_name));
                                }
                            }
                            return results;
                        }
                    }
                }
                vec![]
            });
        }
    }

    let results = futures::future::join_all(futures).await;
    for r in results {
        items.extend(r);
    }

    // Follow-up for magnet links if missing
    let mut follow_up_futures = vec![];
    for (idx, item) in items.iter().enumerate() {
        let magnet = item.get("magnet").and_then(|v| v.as_str()).unwrap_or("");
        let url_path = item
            .get("url")
            .or_else(|| item.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let item_provider = item.get("provider").and_then(|v| v.as_str()).unwrap_or("");

        if magnet.is_empty()
            && !url_path.is_empty()
            && !item_provider.is_empty()
            && let Some(item_conf) = config.get(&format!("{}/item", item_provider)).cloned()
        {
            let client = client.clone();
            let url_path = url_path.to_string();
            follow_up_futures.push(async move {
                let item_url_tpl = item_conf.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let item_url = item_url_tpl.replace("{{item}}", &url_path);

                let mut rb = client.get(&item_url);
                if let Some(headers_obj) = item_conf.get("headers").and_then(|v| v.as_object()) {
                    for (h_key, h_val) in headers_obj {
                        if let Some(h_val_str) = h_val.as_str() {
                            rb = rb.header(h_key, h_val_str);
                        }
                    }
                }

                if let Ok(item_resp) = rb.send().await
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
                                    if let Some(stripped) = a.strip_prefix('@') {
                                        found.value().attr(stripped).unwrap_or("").to_string()
                                    } else {
                                        found.value().attr(a).unwrap_or("").to_string()
                                    }
                                } else {
                                    found
                                        .text()
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                        .trim()
                                        .to_string()
                                };
                                item_data.insert(key.clone(), serde_json::json!(text));
                            }
                        }
                        let mut found_magnet = None;
                        if let Some(magnet_val) = item_data.get("magnet").and_then(|v| v.as_str()) {
                            found_magnet = Some(magnet_val.to_string());
                        } else if let Some(infohash_val) =
                            item_data.get("infohash").and_then(|v| v.as_str())
                        {
                            found_magnet = Some(format!("magnet:?xt=urn:btih:{}", infohash_val));
                        }
                        if let Some(m) = found_magnet {
                            return Some((idx, m));
                        }
                    }
                }
                None
            });
        }
    }

    let follow_ups = futures::future::join_all(follow_up_futures).await;
    for fu in follow_ups.into_iter().flatten() {
        let (idx, magnet_link) = fu;
        if idx < items.len()
            && let Some(obj) = items[idx].as_object_mut()
        {
            obj.insert("magnet".to_string(), serde_json::json!(magnet_link));
        }
    }

    Ok(items)
}
