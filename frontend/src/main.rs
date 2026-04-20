use cloud_torrent_common::{Config, GlobalState, RssSnapshot};
use futures::StreamExt;
use gloo_net::http::{Request, Response};
use gloo_net::websocket::{Message, futures::WebSocket};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::{Event as JsEvent, FileReader};
use yew::prelude::*;

fn copy_text_to_clipboard(text: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };

    let clipboard = window.navigator().clipboard();
    if !wasm_bindgen::JsValue::from(&clipboard).is_undefined() {
        let _ = clipboard.write_text(text);
        return;
    }

    let Some(document) = window.document() else {
        return;
    };
    let Some(body) = document.body() else { return };
    let Ok(el) = document.create_element("textarea") else {
        return;
    };
    let Ok(textarea) = el.dyn_into::<web_sys::HtmlTextAreaElement>() else {
        return;
    };
    textarea.set_value(text);
    textarea.set_read_only(true);
    let style = textarea.style();
    let _ = style.set_property("position", "fixed");
    let _ = style.set_property("left", "-9999px");
    let _ = style.set_property("top", "0");
    if body.append_child(&textarea).is_err() {
        return;
    }
    textarea.select();
    if let Ok(html_doc) = document.dyn_into::<web_sys::HtmlDocument>() {
        let _ = html_doc.exec_command("copy");
    }
    let _ = body.remove_child(&textarea);
}

#[derive(Properties, PartialEq)]
pub struct FileNodeProps {
    pub node: Value,
    pub path: String,
    pub on_delete: Callback<String>,
}

#[function_component(FileNode)]
fn file_node(props: &FileNodeProps) -> Html {
    let expanded = use_state(|| false);
    let name = props
        .node
        .get("Name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let size = props.node.get("Size").and_then(|v| v.as_u64()).unwrap_or(0);
    let children = props.node.get("Children").and_then(|v| v.as_array());
    let is_dir = children.is_some();
    let full_path = if props.path.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", props.path, name)
    };

    let toggle = {
        let expanded = expanded.clone();
        Callback::from(move |_| expanded.set(!*expanded))
    };

    let delete = {
        let on_delete = props.on_delete.clone();
        let full_path = full_path.clone();
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            on_delete.emit(full_path.clone());
        })
    };

    html! {
        <div class="item" style="cursor: pointer;">
            <i onclick={toggle.clone()} class={classes!("icon", if is_dir { if *expanded { "folder open outline" } else { "folder outline" } } else { "file outline" })}></i>
            <div class="content" onclick={toggle}>
                <div class="header" style="display: inline-block;">
                    { name }
                    <span class="ui tiny label" style="margin-left: 10px;">{ format_bytes(size) }</span>
                    if !is_dir {
                        <a href={ format!("/download/{}", full_path) } target="_blank" style="margin-left: 10px;"><i class="download icon"></i></a>
                    } else {
                        <a href={ format!("/download/{}", full_path) } target="_blank" style="margin-left: 10px;"><i class="file archive outline icon"></i></a>
                    }
                    <i onclick={delete} class="red trash icon" style="margin-left: 10px;"></i>
                </div>
                if *expanded {
                    if let Some(children) = children {
                        <div class="list" style="margin-left: 20px; margin-top: 5px;">
                            { for children.iter().map(|child| html! {
                                <FileNode node={child.clone()} path={full_path.clone()} on_delete={props.on_delete.clone()} />
                            }) }
                        </div>
                    }
                }
            </div>
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct ConfigPanelProps {
    pub on_close: Callback<()>,
}

#[function_component(ConfigPanel)]
fn config_panel(props: &ConfigPanelProps) -> Html {
    let config = use_state(|| None::<Config>);
    let saving = use_state(|| false);

    {
        let config = config.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                if let Ok(c) = fetch_config_request().await {
                    config.set(Some(c));
                }
            });
            || ()
        });
    }

    let on_submit = {
        let config = config.clone();
        let saving = saving.clone();
        let on_close = props.on_close.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            if let Some(c) = &*config {
                let c = c.clone();
                let saving = saving.clone();
                let on_close = on_close.clone();
                spawn_local(async move {
                    saving.set(true);
                    let _ = save_config_request(&c).await;
                    saving.set(false);
                    on_close.emit(());
                });
            }
        })
    };

    if let Some(c) = &*config {
        let c = c.clone();
        html! {
            <form class="ui segment edit form" onsubmit={on_submit}>
                <h4 class="ui dividing header">
                    <i class="check square outline icon"></i>
                    { "Configuration" }
                </h4>
                <div class="buttons" style="text-align: center; margin-bottom: 15px;">
                    <button class={classes!("ui", "blue", "button", if *saving { "loading" } else { "" })} type="submit" style="margin-right: 10px;">
                        { "Save" }
                    </button>
                    <div class="ui grey button" onclick={let on_close = props.on_close.clone(); move |_| on_close.emit(())}>
                        { "Cancel" }
                    </div>
                </div>
                <div class="ui horizontal divider">
                    { "Configuration" }
                </div>

                <div class="field">
                    <div class="ui toggle checkbox">
                        <input type="checkbox" checked={c.auto_start} onchange={
                            let config = config.clone();
                            Callback::from(move |_| {
                                if let Some(mut c) = (*config).clone() {
                                    c.auto_start = !c.auto_start;
                                    config.set(Some(c));
                                }
                            })
                        } />
                        <label>{ "Auto Start" }</label>
                    </div>
                    <span title="Whether to start task when added."><i class="question circle icon"></i></span>
                </div>

                <div class="field">
                    <div class="ui toggle checkbox">
                        <input type="checkbox" checked={c.enable_seeding} onchange={
                            let config = config.clone();
                            Callback::from(move |_| {
                                if let Some(mut c) = (*config).clone() {
                                    c.enable_seeding = !c.enable_seeding;
                                    config.set(Some(c));
                                }
                            })
                        } />
                        <label>{ "Enable Seeding" }</label>
                    </div>
                    <span title="Upload even after there's nothing in it for us."><i class="question circle icon"></i></span>
                </div>

                <div class="field">
                    <div class="ui toggle checkbox">
                        <input type="checkbox" checked={c.enable_upload} onchange={
                            let config = config.clone();
                            Callback::from(move |_| {
                                if let Some(mut c) = (*config).clone() {
                                    c.enable_upload = !c.enable_upload;
                                    config.set(Some(c));
                                }
                            })
                        } />
                        <label>{ "Enable Upload" }</label>
                    </div>
                    <span title="Upload data we have."><i class="question circle icon"></i></span>
                </div>

                <div class="field">
                    <div class="ui toggle checkbox">
                        <input type="checkbox" checked={c.disable_trackers} onchange={
                            let config = config.clone();
                            Callback::from(move |_| {
                                if let Some(mut c) = (*config).clone() {
                                    c.disable_trackers = !c.disable_trackers;
                                    config.set(Some(c));
                                }
                            })
                        } />
                        <label>{ "Disable Trackers" }</label>
                    </div>
                    <span title="Don't announce to trackers. This only leaves DHT to discover peers."><i class="question circle icon"></i></span>
                </div>

                <div class="field">
                    <label><h5>{ "Max Concurrent Task " } <span title="Maxmium downloading torrent tasks allowed."><i class="question circle icon"></i></span></h5></label>
                    <input type="number" value={c.max_concurrent_task.to_string()} oninput={
                        let config = config.clone();
                        Callback::from(move |e: InputEvent| {
                            let input: web_sys::HtmlInputElement = e.target_unchecked_into();
                            if let (Ok(p), Some(mut c)) = (input.value().parse(), (*config).clone()) {
                                c.max_concurrent_task = p;
                                config.set(Some(c));
                            }
                        })
                    } />
                </div>

                <div class="field">
                    <label><h5>{ "Seed Ratio " } <span title="The ratio of task Upload/Download data when reached, the task will be stopped."><i class="question circle icon"></i></span></h5></label>
                    <input type="number" step="0.1" value={c.seed_ratio.to_string()} oninput={
                        let config = config.clone();
                        Callback::from(move |e: InputEvent| {
                            let input: web_sys::HtmlInputElement = e.target_unchecked_into();
                            if let (Ok(p), Some(mut c)) = (input.value().parse(), (*config).clone()) {
                                c.seed_ratio = p;
                                config.set(Some(c));
                            }
                        })
                    } />
                </div>

                <div class="field">
                    <label><h5>{ "Upload Rate " } <span title="Upload speed limiter, Low(~50k/s), Medium(~500k/s) and High(~1500k/s) is accepted , Unlimited / 0 or empty result in unlimited rate, or a customed value eg: 850k/720kb/2.85MB."><i class="question circle icon"></i></span></h5></label>
                    <input type="text" value={c.upload_rate.clone()} oninput={
                        let config = config.clone();
                        Callback::from(move |e: InputEvent| {
                            let input: web_sys::HtmlInputElement = e.target_unchecked_into();
                            if let Some(mut c) = (*config).clone() {
                                c.upload_rate = input.value();
                                config.set(Some(c));
                            }
                        })
                    } />
                </div>

                <div class="field">
                    <label><h5>{ "Download Rate " } <span title="Download speed limiter, Low(~50k/s), Medium(~500k/s) and High(~1500k/s) is accepted , Unlimited / 0 or empty result in unlimited rate, or a customed value eg: 850k/720kb/2.85MB."><i class="question circle icon"></i></span></h5></label>
                    <input type="text" value={c.download_rate.clone()} oninput={
                        let config = config.clone();
                        Callback::from(move |e: InputEvent| {
                            let input: web_sys::HtmlInputElement = e.target_unchecked_into();
                            if let Some(mut c) = (*config).clone() {
                                c.download_rate = input.value();
                                config.set(Some(c));
                            }
                        })
                    } />
                </div>

                <div class="field">
                    <label><h5>{ "Tracker List " } <span title="A list of trackers to add to torrents, prefix with &quot;remote:&quot; will be retrived with http."><i class="question circle icon"></i></span></h5></label>
                    <textarea value={c.tracker_list.clone()} oninput={
                        let config = config.clone();
                        Callback::from(move |e: InputEvent| {
                            let input: web_sys::HtmlTextAreaElement = e.target_unchecked_into();
                            if let Some(mut c) = (*config).clone() {
                                c.tracker_list = input.value();
                                config.set(Some(c));
                            }
                        })
                    }
                    />
                </div>

                <div class="field">
                    <div class="ui toggle checkbox">
                        <input type="checkbox" checked={c.always_add_trackers} onchange={
                            let config = config.clone();
                            Callback::from(move |_| {
                                if let Some(mut c) = (*config).clone() {
                                    c.always_add_trackers = !c.always_add_trackers;
                                    config.set(Some(c));
                                }
                            })
                        } />
                        <label>{ "Always Add Trackers" }</label>
                    </div>
                    <span title="Whether add trackers even there are trackers specified in the torrent/magnet"><i class="question circle icon"></i></span>
                </div>

                <div class="field">
                    <label><h5>{ "RSS URL " } <span title="A newline seperated list of magnet RSS feeds. (http/https)"><i class="question circle icon"></i></span></h5></label>
                    <textarea value={c.rss_url.clone()} oninput={
                        let config = config.clone();
                        Callback::from(move |e: InputEvent| {
                            let input: web_sys::HtmlTextAreaElement = e.target_unchecked_into();
                            if let Some(mut c) = (*config).clone() {
                                c.rss_url = input.value();
                                config.set(Some(c));
                            }
                        })
                    }
                    />
                </div>

                <div class="field">
                    <label><h5>{ "Download Directory" }</h5></label>
                    <input type="text" value={c.download_directory.clone()} oninput={
                        let config = config.clone();
                        Callback::from(move |e: InputEvent| {
                            let input: web_sys::HtmlInputElement = e.target_unchecked_into();
                            if let Some(mut c) = (*config).clone() {
                                c.download_directory = input.value();
                                config.set(Some(c));
                            }
                        })
                    } />
                </div>

                <div class="field">
                    <label><h5>{ "Scraper URL" }</h5></label>
                    <input type="text" value={c.scraper_url.clone()} oninput={
                        let config = config.clone();
                        Callback::from(move |e: InputEvent| {
                            let input: web_sys::HtmlInputElement = e.target_unchecked_into();
                            if let Some(mut c) = (*config).clone() {
                                c.scraper_url = input.value();
                                config.set(Some(c));
                            }
                        })
                    } />
                </div>

                <div class="ui horizontal divider">
                    { "Configuration" }
                </div>
                <div class="buttons" style="text-align: center; margin-top: 15px;">
                    <button class={classes!("ui", "blue", "button", if *saving { "loading" } else { "" })} type="submit" style="margin-right: 10px;">
                        { "Save" }
                    </button>
                    <div class="ui grey button" onclick={let on_close = props.on_close.clone(); move |_| on_close.emit(())}>
                        { "Cancel" }
                    </div>
                </div>
            </form>
        }
    } else {
        html! { <div class="ui segment"><div class="ui active centered inline loader"></div></div> }
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let k = 1024.0;
    let sizes = ["B", "KB", "MB", "GB", "TB"];
    let i = (bytes as f64).log(k).floor() as usize;
    format!("{:.2} {}", bytes as f64 / k.powi(i as i32), sizes[i])
}

fn format_timestamp(timestamp: i64) -> String {
    if timestamp <= 0 {
        return "".to_string();
    }

    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

async fn parse_error_response(resp: Response) -> String {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if let Ok(json) = serde_json::from_str::<Value>(&text)
        && let Some(message) = json.get("error").and_then(|value| value.as_str())
    {
        return message.to_string();
    }
    if text.trim().is_empty() {
        format!("Request failed with HTTP {}", status)
    } else {
        text
    }
}

async fn post_text_request(url: &str, body: String) -> Result<(), String> {
    let resp = Request::post(url)
        .body(body)
        .map_err(|err| err.to_string())?
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if (200..300).contains(&resp.status()) {
        Ok(())
    } else {
        Err(parse_error_response(resp).await)
    }
}

async fn post_bytes_request(url: &str, body: Vec<u8>) -> Result<(), String> {
    let resp = Request::post(url)
        .body(body)
        .map_err(|err| err.to_string())?
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if (200..300).contains(&resp.status()) {
        Ok(())
    } else {
        Err(parse_error_response(resp).await)
    }
}

async fn fetch_rss_snapshot(refresh: bool) -> Result<RssSnapshot, String> {
    let url = if refresh { "/rss?refresh=true" } else { "/rss" };
    let resp = Request::get(url)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if (200..300).contains(&resp.status()) {
        resp.json::<RssSnapshot>()
            .await
            .map_err(|err| err.to_string())
    } else {
        Err(parse_error_response(resp).await)
    }
}

async fn post_rss_load_request(item_id: String, load_url: String) -> Result<RssSnapshot, String> {
    let resp = Request::post("/api/rss/load")
        .json(&serde_json::json!({
            "item_id": item_id,
            "load_url": load_url,
        }))
        .map_err(|err| err.to_string())?
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if (200..300).contains(&resp.status()) {
        resp.json::<RssSnapshot>()
            .await
            .map_err(|err| err.to_string())
    } else {
        Err(parse_error_response(resp).await)
    }
}

async fn fetch_config_request() -> Result<Config, String> {
    let resp = Request::get("/api/configure")
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if (200..300).contains(&resp.status()) {
        resp.json::<Config>().await.map_err(|err| err.to_string())
    } else {
        Err(parse_error_response(resp).await)
    }
}

async fn save_config_request(config: &Config) -> Result<(), String> {
    let resp = Request::post("/api/configure")
        .json(config)
        .map_err(|err| err.to_string())?
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if (200..300).contains(&resp.status()) {
        Ok(())
    } else {
        Err(parse_error_response(resp).await)
    }
}

#[derive(Clone, PartialEq, Debug)]
enum OmniMode {
    Search,
    Magnet,
    Torrent,
}

#[function_component(App)]
fn app() -> Html {
    let state = use_state(|| None::<GlobalState>);
    let connected = use_state(|| false);
    let torrents_expanded = use_state(|| true);
    let downloads_expanded = use_state(|| false);
    let expanded_files = use_state(HashSet::<String>::new);
    let expanded_panels = use_state(HashMap::<String, String>::new);
    let omni_input = use_state(String::new);
    let omni_mode = use_state(|| OmniMode::Search);
    let files = use_state(|| None::<Value>);
    let rss_state = use_state(|| None::<RssSnapshot>);
    let rss_config = use_state(|| None::<Config>);
    let rss_input = use_state(String::new);

    let show_config = use_state(|| false);
    let show_omni_editor = use_state(|| false);
    let show_rss = use_state(|| false);
    let show_engine_status = use_state(|| false);
    let rss_loading = use_state(|| false);
    let rss_config_loading = use_state(|| false);
    let rss_config_saving = use_state(|| false);
    let searching = use_state(|| false);
    let search_results = use_state(Vec::<serde_json::Value>::new);
    let providers = use_state(HashMap::<String, Value>::new);
    let selected_provider = use_state(|| "thepiratebay".to_string());
    let ui_error = use_state(|| None::<String>);
    let last_rss_error = use_state(String::new);

    {
        let state = state.clone();
        let connected = connected.clone();
        use_effect_with((), move |_| {
            let window = web_sys::window().unwrap();
            let location = window.location();
            let host = location.host().unwrap();
            let protocol = if location.protocol().unwrap() == "https:" {
                "wss:"
            } else {
                "ws:"
            };
            let url = format!("{}//{}/sync/ws", protocol, host);

            let mut ws = WebSocket::open(&url).expect("Failed to open WebSocket");

            spawn_local(async move {
                connected.set(true);
                while let Some(msg) = ws.next().await {
                    if let Some(gs) = msg.ok().and_then(|m| {
                        if let Message::Text(text) = m {
                            serde_json::from_str::<GlobalState>(&text).ok()
                        } else {
                            None
                        }
                    }) {
                        state.set(Some(gs));
                    }
                }
                connected.set(false);
            });

            || ()
        });
    }

    {
        let providers = providers.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                if let Ok(p) = async {
                    if let Ok(resp) = Request::get("/api/searchproviders").send().await {
                        resp.json::<HashMap<String, Value>>().await
                    } else {
                        Err(gloo_net::Error::GlooError("Request failed".into()))
                    }
                }
                .await
                {
                    providers.set(p);
                }
            });
            || ()
        });
    }

    {
        let ui_error = ui_error.clone();
        let last_rss_error = last_rss_error.clone();
        let rss_error = (*state)
            .as_ref()
            .map(|value| value.rss_last_error.clone())
            .unwrap_or_default();
        use_effect_with(rss_error, move |rss_error| {
            if rss_error.is_empty() {
                last_rss_error.set(String::new());
            } else if *last_rss_error != *rss_error {
                ui_error.set(Some(format!("RSS subscription error: {}", rss_error)));
                last_rss_error.set(rss_error.clone());
            }
            || ()
        });
    }

    let refresh_files = {
        let files = files.clone();
        Callback::from(move |_| {
            let files = files.clone();
            spawn_local(async move {
                if let Ok(json) = async {
                    if let Ok(resp) = Request::get("/api/files").send().await {
                        resp.json::<Value>().await
                    } else {
                        Err(gloo_net::Error::GlooError("Request failed".into()))
                    }
                }
                .await
                {
                    files.set(Some(json));
                }
            });
        })
    };

    let fetch_rss = {
        let rss_loading = rss_loading.clone();
        let rss_state = rss_state.clone();
        let ui_error = ui_error.clone();
        let last_rss_error = last_rss_error.clone();
        Callback::from(move |refresh: bool| {
            let rss_loading = rss_loading.clone();
            let rss_state = rss_state.clone();
            let ui_error = ui_error.clone();
            let last_rss_error = last_rss_error.clone();
            spawn_local(async move {
                rss_loading.set(true);
                match fetch_rss_snapshot(refresh).await {
                    Ok(json) => {
                        if !json.last_error.is_empty() {
                            last_rss_error.set(json.last_error.clone());
                            ui_error
                                .set(Some(format!("RSS subscription error: {}", json.last_error)));
                        } else {
                            last_rss_error.set(String::new());
                        }
                        rss_state.set(Some(json));
                    }
                    Err(err) => {
                        ui_error.set(Some(format!("Failed to refresh RSS feeds: {}", err)));
                    }
                }
                rss_loading.set(false);
            });
        })
    };

    {
        let fetch_rss = fetch_rss.clone();
        let show_rss = show_rss.clone();
        let rss_trigger = (*state)
            .as_ref()
            .map(|value| format!("{}|{}", value.latest_rss_guid, value.rss_last_error))
            .unwrap_or_default();
        use_effect_with((rss_trigger, *show_rss), move |(_, show_rss)| {
            if *show_rss {
                fetch_rss.emit(false);
            }
            || ()
        });
    }

    {
        let show_rss = show_rss.clone();
        let rss_config = rss_config.clone();
        let rss_input = rss_input.clone();
        let rss_config_loading = rss_config_loading.clone();
        let ui_error = ui_error.clone();
        use_effect_with(*show_rss, move |show_rss| {
            if *show_rss {
                let rss_config = rss_config.clone();
                let rss_input = rss_input.clone();
                let rss_config_loading = rss_config_loading.clone();
                let ui_error = ui_error.clone();
                spawn_local(async move {
                    rss_config_loading.set(true);
                    match fetch_config_request().await {
                        Ok(config) => {
                            rss_input.set(config.rss_url.clone());
                            rss_config.set(Some(config));
                        }
                        Err(err) => {
                            ui_error.set(Some(format!("Failed to load RSS settings: {}", err)))
                        }
                    }
                    rss_config_loading.set(false);
                });
            }
            || ()
        });
    }

    let on_delete_file = {
        let refresh_files = refresh_files.clone();
        Callback::from(move |path: String| {
            let refresh_files = refresh_files.clone();
            spawn_local(async move {
                let _ = Request::delete(&format!("/download/{}", path)).send().await;
                refresh_files.emit(());
            });
        })
    };

    let save_rss_subscriptions = {
        let rss_config = rss_config.clone();
        let rss_input = rss_input.clone();
        let rss_config_saving = rss_config_saving.clone();
        let ui_error = ui_error.clone();
        let fetch_rss = fetch_rss.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            let Some(mut config) = (*rss_config).clone() else {
                ui_error.set(Some("RSS settings are still loading.".to_string()));
                return;
            };
            let rss_value = (*rss_input).clone();
            let rss_config = rss_config.clone();
            let rss_config_saving = rss_config_saving.clone();
            let ui_error = ui_error.clone();
            let fetch_rss = fetch_rss.clone();
            spawn_local(async move {
                rss_config_saving.set(true);
                config.rss_url = rss_value;
                match save_config_request(&config).await {
                    Ok(()) => {
                        rss_config.set(Some(config));
                        fetch_rss.emit(true);
                    }
                    Err(err) => {
                        ui_error.set(Some(format!("Failed to save RSS subscriptions: {}", err)));
                    }
                }
                rss_config_saving.set(false);
            });
        })
    };

    let parse_omni = {
        let omni_input = omni_input.clone();
        let omni_mode = omni_mode.clone();
        Callback::from(move |val: String| {
            if val.starts_with("magnet:") || val.len() == 40 {
                omni_mode.set(OmniMode::Magnet);
            } else if val.starts_with("http") && val.contains(".torrent") {
                omni_mode.set(OmniMode::Torrent);
            } else {
                omni_mode.set(OmniMode::Search);
            }
            omni_input.set(val);
        })
    };

    let on_omni_submit = {
        let omni_input = omni_input.clone();
        let omni_mode = omni_mode.clone();
        let searching = searching.clone();
        let search_results = search_results.clone();
        let selected_provider = selected_provider.clone();
        let ui_error = ui_error.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            let val = (*omni_input).clone();
            if val.is_empty() {
                return;
            }
            let omni_input = omni_input.clone();
            let omni_mode = (*omni_mode).clone();
            let searching = searching.clone();
            let search_results = search_results.clone();
            let provider = (*selected_provider).clone();
            let ui_error = ui_error.clone();
            spawn_local(async move {
                match omni_mode {
                    OmniMode::Magnet | OmniMode::Torrent => {
                        match post_text_request("/api/magnet", val).await {
                            Ok(()) => omni_input.set(String::new()),
                            Err(err) => ui_error.set(Some(err)),
                        }
                    }
                    OmniMode::Search => {
                        searching.set(true);
                        if let Ok(res) = async {
                            if let Ok(resp) = Request::get(&format!(
                                "/api/search?query={}&provider={}",
                                val, provider
                            ))
                            .send()
                            .await
                            {
                                resp.json::<Vec<serde_json::Value>>().await
                            } else {
                                Err(gloo_net::Error::GlooError("Request failed".into()))
                            }
                        }
                        .await
                        {
                            search_results.set(res);
                        }
                        searching.set(false);
                    }
                }
            });
        })
    };

    let on_load_submit = {
        let omni_input = omni_input.clone();
        let ui_error = ui_error.clone();
        Callback::from(move |_| {
            let val = (*omni_input).clone();
            if val.is_empty() {
                return;
            }
            let omni_input = omni_input.clone();
            let ui_error = ui_error.clone();
            spawn_local(async move {
                match post_text_request("/api/magnet", val).await {
                    Ok(()) => omni_input.set(String::new()),
                    Err(err) => ui_error.set(Some(err)),
                }
            });
        })
    };

    let on_search_submit = {
        let omni_input = omni_input.clone();
        let searching = searching.clone();
        let search_results = search_results.clone();
        let selected_provider = selected_provider.clone();
        Callback::from(move |_| {
            let val = (*omni_input).clone();
            if val.is_empty() {
                return;
            }
            let searching = searching.clone();
            let search_results = search_results.clone();
            let provider = (*selected_provider).clone();
            spawn_local(async move {
                searching.set(true);
                if let Ok(res) = async {
                    if let Ok(resp) =
                        Request::get(&format!("/api/search?query={}&provider={}", val, provider))
                            .send()
                            .await
                    {
                        resp.json::<Vec<serde_json::Value>>().await
                    } else {
                        Err(gloo_net::Error::GlooError("Request failed".into()))
                    }
                }
                .await
                {
                    search_results.set(res);
                }
                searching.set(false);
            });
        })
    };

    let on_drop = {
        let ui_error = ui_error.clone();
        Callback::from(move |e: DragEvent| {
            e.prevent_default();
            let ui_error = ui_error.clone();
            if let Some(files) = e.data_transfer().and_then(|dt| dt.files()) {
                for i in 0..files.length() {
                    if let Some(file) = files.get(i) {
                        let reader = FileReader::new().unwrap();
                        let reader_c = reader.clone();
                        let ui_error = ui_error.clone();
                        let onload = Closure::wrap(Box::new(move |_e: JsEvent| {
                            let result = reader_c.result().unwrap();
                            let array = js_sys::Uint8Array::new(&result);
                            let bytes = array.to_vec();
                            let ui_error = ui_error.clone();
                            spawn_local(async move {
                                if let Err(err) = post_bytes_request("/api/torrent", bytes).await {
                                    ui_error.set(Some(err));
                                }
                            });
                        })
                            as Box<dyn FnMut(JsEvent)>);
                        reader.set_onload(Some(onload.as_ref().unchecked_ref()));
                        reader.read_as_array_buffer(&file).unwrap();
                        onload.forget();
                    }
                }
            }
        })
    };

    let on_start = |hash: String| {
        let ui_error = ui_error.clone();
        Callback::from(move |_| {
            let hash = hash.clone();
            let ui_error = ui_error.clone();
            spawn_local(async move {
                if let Err(err) = post_text_request("/api/magnet", format!("start:{}", hash)).await
                {
                    ui_error.set(Some(err));
                }
            });
        })
    };

    let on_remove = |hash: String| {
        Callback::from(move |_| {
            let hash = hash.clone();
            spawn_local(async move {
                let _ = Request::post("/api/magnet")
                    .body(format!("delete:{}", hash))
                    .unwrap()
                    .send()
                    .await;
            });
        })
    };

    let on_stop = |hash: String| {
        Callback::from(move |_| {
            let hash = hash.clone();
            spawn_local(async move {
                let _ = Request::post("/api/magnet")
                    .body(format!("stop:{}", hash))
                    .unwrap()
                    .send()
                    .await;
            });
        })
    };
    let toggle_files = {
        let expanded_files = expanded_files.clone();
        Callback::from(move |hash: String| {
            let mut current = (*expanded_files).clone();
            if current.contains(&hash) {
                current.remove(&hash);
            } else {
                current.insert(hash);
            }
            expanded_files.set(current);
        })
    };

    let toggle_panel = {
        let expanded_panels = expanded_panels.clone();
        Callback::from(move |(hash, panel): (String, String)| {
            let mut current = (*expanded_panels).clone();
            if current.get(&hash) == Some(&panel) {
                current.remove(&hash);
            } else {
                current.insert(hash, panel);
            }
            expanded_panels.set(current);
        })
    };

    let is_connected = *connected;

    html! {
        <div class="cage" ondragover={Callback::from(|e: DragEvent| e.prevent_default())} ondrop={on_drop}>
            if !is_connected {
                <div class="connect-warning ui inverted masthead center aligned segment">
                    <div class="ui text container">
                        <h1 class="ui inverted header">{ "Connecting" }</h1>
                        <h1 class="ui inverted header"><i class="red lightning icon"></i></h1>
                    </div>
                </div>
            }

            if let Some(error) = &*ui_error {
                <div style="position: fixed; top: 16px; right: 16px; z-index: 2000; max-width: min(520px, calc(100vw - 32px));">
                    <div class="ui negative message">
                        <i class="close icon" style="cursor: pointer;" onclick={let ui_error = ui_error.clone(); move |_| ui_error.set(None)}></i>
                        <div class="header">{ "Action Failed" }</div>
                        <p style="white-space: pre-wrap; margin-bottom: 0;">{ error.clone() }</p>
                    </div>
                </div>
            }

            <div class="title">
                <h2>
                    <a href="https://github.com/OctopusTakopi/cloud-torrent-rs" target="_blank">
                        <i class="blue cloud icon"></i> { " Cloud Torrent-rs" }
                    </a>
                </h2>
                <div class="status">
                    <i class={classes!("ui", "circular", "rss", "square", "icon", if *show_rss { "green" } else { "" })} onclick={let show_rss = show_rss.clone(); let fetch_rss = fetch_rss.clone(); move |_| { let ns = !*show_rss; show_rss.set(ns); if ns { fetch_rss.emit(true); } }} title="RSS List"></i>
                    <i class={classes!("ui", "circular", "server", "icon", if *show_config { "green" } else { "" })} onclick={let show_config = show_config.clone(); move |_| show_config.set(!*show_config)} title="Edit Config"></i>
                    <i class={classes!("ui", "circular", "magnet", "icon", if *show_omni_editor { "green" } else { "blue" })} onclick={let show_omni_editor = show_omni_editor.clone(); move |_| show_omni_editor.set(!*show_omni_editor)} title="Edit Magnet/Torrent"></i>
                </div>
            </div>

            if let Some(gs) = &*state {
                <div class="ui container">
                    if *show_config {
                        <ConfigPanel on_close={let show_config = show_config.clone(); move |_| show_config.set(false)} />
                    }

                    if *show_omni_editor {
                        <div class="ui segment">
                            <h4 class="ui dividing header"><i class="sticky note outline icon"></i>{ " Magnet URI Editor" }</h4>
                            <div class="ui form">
                                <div class="field">
                                    <label>{ "Raw Magnet" }</label>
                                    <textarea placeholder="Paste your magnet here..." onchange={let omni_input = omni_input.clone(); Callback::from(move |e: Event| {
                                        let input: web_sys::HtmlTextAreaElement = e.target_unchecked_into();
                                        omni_input.set(input.value());
                                    })} />
                                </div>
                                <button class="ui blue button" onclick={let on_load_submit = on_load_submit.clone(); move |_| on_load_submit.emit(())}>{ "Load" }</button>
                            </div>
                        </div>
                    }

                    // Omni Bar
                    <div class="omni search">
                        <div class="ui fluid icon input">
                            <input
                                type="text"
                                placeholder="Enter magnet URI, torrent URL or search query..."
                                value={(*omni_input).clone()}
                                oninput={let parse_omni = parse_omni.clone(); Callback::from(move |e: InputEvent| {
                                    let input: web_sys::HtmlInputElement = e.target_unchecked_into();
                                    parse_omni.emit(input.value());
                                })}
                                onkeydown={let on_omni_submit = on_omni_submit.clone(); move |e: KeyboardEvent| { if e.key() == "Enter" { on_omni_submit.emit(SubmitEvent::new("").unwrap()); } }}
                            />
                            <div class="icon-wrapper" onclick={let on_load_submit = on_load_submit.clone(); move |_| on_load_submit.emit(())}>
                                <i class={classes!("icon", match *omni_mode {
                                    OmniMode::Magnet => "magnet blue",
                                    OmniMode::Torrent => "file teal",
                                    OmniMode::Search => "search",
                                })} title="Load"></i>
                            </div>
                        </div>

                        if !omni_input.is_empty() {
                            <div class="ui raised segment" style="margin-top: 10px; text-align: center;">
                                <div class="ui action input" style="display: inline-flex; width: auto; max-width: 100%;">
                                    if *omni_mode == OmniMode::Search {
                                        <select class="ui compact selection dropdown" style="width: auto; border-radius: .28571429rem 0 0 .28571429rem; font-size: 0.85rem; padding: 0.5em;" onchange={let selected_provider = selected_provider.clone(); Callback::from(move |e: Event| { let input: web_sys::HtmlSelectElement = e.target_unchecked_into(); selected_provider.set(input.value()); })}>
                                            { for providers.iter().map(|(k, v)| html! {
                                                <option value={k.clone()} selected={*selected_provider == *k}>{ v.get("name").and_then(|v| v.as_str()).unwrap_or(k) }</option>
                                            }) }
                                        </select>
                                        <button class={classes!("ui", "button", if *searching { "loading" } else { "" })} onclick={let on_search_submit = on_search_submit.clone(); move |_| on_search_submit.emit(())} style="border-radius: 0 .28571429rem .28571429rem 0; font-size: 0.85rem;">
                                            <i class="search icon"></i>
                                            { " Search" }
                                        </button>
                                    } else {
                                        <button class="ui blue button" onclick={let on_load_submit = on_load_submit.clone(); move |_| on_load_submit.emit(())}>
                                            <i class="magnet icon"></i>
                                            { " Load" }
                                        </button>
                                    }
                                </div>
                            </div>
                        }
                    </div>

                    if !search_results.is_empty() || *searching {
                        <div class="ui segment" id="omni_search_results">
                            <div class="result_header">
                                <span class="ui header">{ "Search Results" }</span>
                                <i class="close icon close_icon" style="float: right; cursor: pointer;" onclick={let search_results = search_results.clone(); move |_| search_results.set(vec![])}></i>
                            </div>
                            <div class="results">
                                <table class="ui unstackable compact striped table">
                                    <thead>
                                        <tr><th>{ "Name" }</th><th>{ "Size" }</th><th>{ "S/P" }</th><th>{ "Action" }</th></tr>
                                    </thead>
                                    <tbody>
                                        { for search_results.iter().map(|res| {
                                            let str_field = |k: &str| res.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                                            let magnet = str_field("magnet");
                                            let name   = str_field("name");
                                            let size   = str_field("size");
                                            let seeds  = str_field("seeds");
                                            let peers  = str_field("peers");
                                            let ui_error = ui_error.clone();
                                            html! {
                                                <tr>
                                                    <td class="name">{ name }</td>
                                                    <td class="right aligned">{ size }</td>
                                                    <td>{ format!("{}/{}", seeds, peers) }</td>
                                                    <td>
                                                        <button class="ui tiny blue icon button" onclick={let magnet = magnet.clone(); Callback::from(move |_| {
                                                            let magnet = magnet.clone();
                                                            let ui_error = ui_error.clone();
                                                            spawn_local(async move {
                                                                if let Err(err) = post_text_request("/api/magnet", magnet).await {
                                                                    ui_error.set(Some(err));
                                                                }
                                                            });
                                                        })}><i class="plus icon"></i></button>
                                                    </td>
                                                </tr>
                                            }
                                        }) }
                                    </tbody>
                                </table>
                            </div>
                        </div>
                    }

                    if *show_rss {
                        <div class="ui segment" id="rss_results">
                            <div class="result_header">
                                <span class="ui header">
                                    <i class="rss square icon"></i>
                                    {
                                        if let Some(rss) = &*rss_state {
                                            format!(" RSS ({})", rss.items.len())
                                        } else {
                                            " RSS".to_string()
                                        }
                                    }
                                </span>
                                <div style="float: right; display: flex; gap: 8px; align-items: center;">
                                    <button
                                        class={classes!("ui", "tiny", "teal", "button", if *rss_loading { "loading" } else { "" })}
                                        onclick={let fetch_rss = fetch_rss.clone(); move |_| fetch_rss.emit(true)}
                                    >
                                        <i class="redo icon"></i>
                                        { "Refresh" }
                                    </button>
                                    <i
                                        class="close icon close_icon"
                                        style="cursor: pointer;"
                                        onclick={let show_rss = show_rss.clone(); move |_| show_rss.set(false)}
                                    ></i>
                                </div>
                            </div>

                            <div class="ui form" style="margin-bottom: 16px;">
                                <div class="field">
                                    <label>{ "RSS Subscriptions" }</label>
                                    <textarea
                                        rows="4"
                                        placeholder="One RSS feed URL per line"
                                        value={(*rss_input).clone()}
                                        disabled={*rss_config_loading || *rss_config_saving}
                                        oninput={let rss_input = rss_input.clone(); Callback::from(move |e: InputEvent| {
                                            let input: web_sys::HtmlTextAreaElement = e.target_unchecked_into();
                                            rss_input.set(input.value());
                                        })}
                                    />
                                    <div style="display: flex; gap: 8px; align-items: center; margin-top: 8px; flex-wrap: wrap;">
                                        <button
                                            class={classes!("ui", "tiny", "blue", "button", if *rss_config_saving { "loading" } else { "" })}
                                            disabled={*rss_config_loading || *rss_config_saving}
                                            onclick={save_rss_subscriptions.clone()}
                                        >
                                            <i class="save icon"></i>
                                            { "Save Feeds" }
                                        </button>
                                        <button
                                            class="ui tiny button"
                                            type="button"
                                            onclick={let show_config = show_config.clone(); move |_| show_config.set(true)}
                                        >
                                            <i class="settings icon"></i>
                                            { "Full Settings" }
                                        </button>
                                        if *rss_config_loading {
                                            <span class="ui tiny text">{ "Loading RSS settings..." }</span>
                                        } else {
                                            <span class="ui tiny text">{ "Save applies the new feed list immediately." }</span>
                                        }
                                    </div>
                                </div>
                            </div>

                            if let Some(rss) = &*rss_state {
                                <div style="margin-bottom: 10px;">
                                    <span class="ui tiny label">
                                        <i class="rss icon"></i>
                                        { format!("Feeds: {}", rss.feed_count) }
                                    </span>
                                    if rss.last_updated > 0 {
                                        <span class="ui tiny label">
                                            <i class="clock outline icon"></i>
                                            { format!("Updated {}", format_timestamp(rss.last_updated)) }
                                        </span>
                                    }
                                </div>

                                if !rss.last_error.is_empty() {
                                    <div class="ui warning message">
                                        <pre style="white-space: pre-wrap; margin: 0;">{ rss.last_error.clone() }</pre>
                                    </div>
                                }

                                if rss.items.is_empty() && !*rss_loading {
                                    <div class="ui message">
                                        <p>{ "No RSS items yet. Configure one or more RSS URLs and refresh." }</p>
                                    </div>
                                } else {
                                    <div class="results">
                                        <table class="ui unstackable compact striped table">
                                            <thead>
                                                <tr>
                                                    <th>{ "Name" }</th>
                                                    <th>{ "Feed" }</th>
                                                    <th>{ "Published" }</th>
                                                    <th>{ "Action" }</th>
                                                </tr>
                                            </thead>
                                            <tbody>
                                                { for rss.items.iter().map(|item| {
                                                    let link = item.link.clone();
                                                    let load_url = item.load_url.clone();
                                                    let item_id = item.id.clone();
                                                    let rss_state = rss_state.clone();
                                                    let ui_error = ui_error.clone();
                                                    let is_magnet = load_url.starts_with("magnet:");
                                                    html! {
                                                        <tr>
                                                            <td class="name">
                                                                if link.starts_with("http://") || link.starts_with("https://") {
                                                                    <a href={link} target="_blank">{ item.title.clone() }</a>
                                                                } else {
                                                                    <>
                                                                        { item.title.clone() }
                                                                    </>
                                                                }
                                                                if item.is_new {
                                                                    <span class="ui tiny teal label" style="margin-left: 8px;">{ "new" }</span>
                                                                }
                                                            </td>
                                                            <td>{ if item.source_title.is_empty() { item.source_url.clone() } else { item.source_title.clone() } }</td>
                                                            <td>{ if item.published.is_empty() { "-".to_string() } else { item.published.clone() } }</td>
                                                            <td>
                                                                <button
                                                                    class={classes!("ui", "tiny", if item.loaded { "green" } else { "blue" }, "icon", "button")}
                                                                    disabled={item.loaded}
                                                                    onclick={Callback::from(move |_| {
                                                                        let load_url = load_url.clone();
                                                                        let item_id = item_id.clone();
                                                                        let rss_state = rss_state.clone();
                                                                        let ui_error = ui_error.clone();
                                                                        spawn_local(async move {
                                                                            match post_rss_load_request(item_id, load_url).await {
                                                                                Ok(snapshot) => rss_state.set(Some(snapshot)),
                                                                                Err(err) => ui_error.set(Some(err)),
                                                                            }
                                                                        });
                                                                    })}
                                                                >
                                                                    <i class={if item.loaded { "check icon" } else if is_magnet { "magnet icon" } else { "file download icon" }}></i>
                                                                </button>
                                                            </td>
                                                        </tr>
                                                    }
                                                }) }
                                            </tbody>
                                        </table>
                                    </div>
                                }
                            } else if *rss_loading {
                                <div class="ui active centered inline loader"></div>
                            } else {
                                <div class="ui message">
                                    <p>{ "No RSS data loaded yet." }</p>
                                </div>
                            }
                        </div>
                    }

                    // Torrents Section
                    <div class="ui grid section-header" onclick={let torrents_expanded = torrents_expanded.clone(); Callback::from(move |_| torrents_expanded.set(!*torrents_expanded))}>
                        <div class="column">
                            <i class={classes!("square", "outline", "icon", if *torrents_expanded { "minus" } else { "plus" })}></i>
                            <span class="ui header">
                                { format!(" Torrents ({})", gs.torrents.len()) }
                                <span class="ui label">
                                    <i class="list icon"></i>
                                    { format!(" ▲: {} ▼: {}", format_bytes(gs.stats.conn_stat.bytes_written_data), format_bytes(gs.stats.conn_stat.bytes_read_useful_data)) }
                                </span>
                            </span>
                        </div>
                    </div>

                    if *torrents_expanded {
                        <div class="ui raised segments">
                            if gs.torrents.is_empty() {
                                <div class="ui message nodownloads"><p>{ "Add torrents or magnet to download" }</p></div>
                            }
                            { for gs.torrents.iter().map(|t| {
                                let hash = t.info_hash.clone();
                                let is_files_open = expanded_files.contains(&hash);
                                html! {
                                    <div class={classes!("ui", "torrent", "segment", if is_files_open { "open" } else { "" })}>
                                        <div class="ui stackable grid">
                                            <div class="ten wide info column">
                                                <div class="name ui header">
                                                    if !t.loaded {
                                                        <span style="color: grey; word-break: break-all;">{ &t.magnet }</span>
                                                    } else {
                                                        { &t.name }
                                                    }
                                                </div>
                                                <div class="speed">
                                                    <span class={classes!("ui", "label",
                                                        if t.upload_rate > 0.0 && t.upload_rate < 102400.0 { "yellow" }
                                                        else if t.upload_rate >= 102400.0 { "green" }
                                                        else { "" })} onclick={let hash = hash.clone(); toggle_panel.reform(move |_| (hash.clone(), "UpSpeed".to_string()))}>
                                                        <i class="cloud upload icon"></i>{ format!("{}/s", format_bytes(t.upload_rate as u64)) }
                                                    </span>
                                                    <span class={classes!("ui", "label",
                                                        if t.download_rate > 0.0 && t.download_rate < 102400.0 { "yellow" }
                                                        else if t.download_rate >= 102400.0 { "green" }
                                                        else { "" })} onclick={let hash = hash.clone(); toggle_panel.reform(move |_| (hash.clone(), "DownSpeed".to_string()))}>
                                                        <i class="cloud download icon"></i>{ format!("{}/s", format_bytes(t.download_rate as u64)) }
                                                    </span>
                                                    <span class={classes!("ui", "label",
                                                        if t.peers_connected > 0 { "green" }
                                                        else if t.peers_total > 0 { "yellow" }
                                                        else { "" })} onclick={let hash = hash.clone(); toggle_panel.reform(move |_| (hash.clone(), "Peers".to_string()))}>
                                                        <i class="sitemap icon"></i>{ format!("{}/{}", t.peers_connected, t.peers_total) }
                                                    </span>
                                                    <span class={classes!("ui", "label",
                                                        if t.seed_ratio >= 1.0 { "green" }
                                                        else if t.seed_ratio > 0.0 { "yellow" }
                                                        else { "" })} onclick={let hash = hash.clone(); toggle_panel.reform(move |_| (hash.clone(), "Ratio".to_string()))}>
                                                        <i class="exchange icon"></i>{ format!("{:.2}", t.seed_ratio) }
                                                    </span>
                                                </div>
                                                <div class={classes!("ui",
                                                    if t.percent >= 100.0 || t.is_seeding { "green" } else { "blue" },
                                                    if t.started && t.percent > 0.0 && t.percent < 100.0 { "active" } else { "" },
                                                    "small", "progress")}>
                                                    <div class="bar" style={ format!("width: {}%; background-color: {};",
                                                        if t.percent < 1.0 && t.percent > 0.0 { 1.0 } else { t.percent },
                                                        if t.percent >= 100.0 || t.is_seeding { "#21ba45" } else { "#2185d0" }
                                                    ) }>
                                                        <div class="centered progress">{ format!("{:.1}%", t.percent) }</div>
                                                    </div>
                                                </div>
                                            </div>
                                            <div class="six wide controls column">
                                                <div class="ui mini buttons">
                                                    <button class={classes!("ui", "button", if is_files_open { "teal" } else { "blue" })} onclick={let hash = hash.clone(); toggle_files.reform(move |_| hash.clone())}>
                                                        <i class="file icon"></i>{ " Files" }
                                                    </button>
                                                    if t.loaded {
                                                        <button class={classes!("ui", "compact", "button", if !t.started { "green" } else { "" })} disabled={t.started} onclick={on_start(hash.clone())}>
                                                            <i class="play icon"></i>{ " Start" }
                                                        </button>
                                                    }
                                                    if t.started && t.loaded {
                                                        <button class="ui red compact button" onclick={on_stop(hash.clone())}><i class="stop icon"></i>{ " Stop" }</button>
                                                    }
                                                    if !t.loaded {
                                                        <button class="ui red compact button" onclick={on_remove(hash.clone())}><i class="trash icon"></i>{ " Remove" }</button>
                                                    }
                                                    if t.loaded && !t.started {
                                                        <button class="ui orange compact button" onclick={on_remove(hash.clone())}><i class="trash icon"></i>{ " Remove" }</button>
                                                    }
                                                </div>
                                                if t.started {
                                                    <div class="status download" style="margin-top: 10px;">
                                                        <span class="ui label" title="Downloaded" onclick={let hash = hash.clone(); toggle_panel.reform(move |_| (hash.clone(), "Downloaded".to_string()))}>
                                                            <i class="save icon"></i>
                                                            { format!("{} / {}", format_bytes(t.downloaded as u64), format_bytes(t.size as u64)) }
                                                        </span>
                                                    </div>
                                                }
                                            </div>
                                        </div>
                                        if let Some(panel) = expanded_panels.get(&hash) {
                                            <div class="ui sixteen wide column torrentinfobar">
                                                <div class="ui info message" style="margin-bottom: 10px;">
                                                    <i class="close icon" onclick={let hash = hash.clone(); let p = panel.clone(); toggle_panel.reform(move |_| (hash.clone(), p.clone()))}></i>
                                                    <h3 class="header">
                                                        <i class="tags icon"></i>
                                                        { panel }
                                                    </h3>
                                                    <p></p>
                                                    if panel == "UpSpeed" {
                                                        <div class="ui blue basic label">
                                                            <i class="tachometer alternate icon"></i>
                                                            { "Upload Speed" }
                                                            <div class="detail">{ format!("{}/s", format_bytes(t.upload_rate as u64)) }</div>
                                                        </div>
                                                        <div class="ui blue basic label">
                                                            <i class="cloud upload icon"></i>
                                                            { "Uploaded Data" }
                                                            <div class="detail">{ format_bytes(t.uploaded as u64) }</div>
                                                        </div>
                                                    } else if panel == "DownSpeed" {
                                                        <div class="ui blue basic label">
                                                            <i class="tachometer alternate icon"></i>
                                                            { "Download Speed" }
                                                            <div class="detail">{ format!("{}/s", format_bytes(t.download_rate as u64)) }</div>
                                                        </div>
                                                        <div class="ui blue basic label">
                                                            <i class="cloud download icon"></i>
                                                            { "Downloaded Data" }
                                                            <div class="detail">{ format_bytes(t.downloaded as u64) }</div>
                                                        </div>
                                                    } else if panel == "Peers" {
                                                        <div class="ui basic blue label">
                                                            <i class="users icon"></i>
                                                            { "Total" }
                                                            <div class="detail">{ t.peers_total }</div>
                                                        </div>
                                                        <div class="ui basic blue label">
                                                            <i class="user icon"></i>
                                                            { "Active" }
                                                            <div class="detail">{ t.peers_connected }</div>
                                                        </div>
                                                        <div class="ui basic blue label">
                                                            <i class="user md icon"></i>
                                                            { "HalfOpen" }
                                                            <div class="detail">{ t.peers_half_open }</div>
                                                        </div>
                                                        <div class="ui basic blue label">
                                                            <i class="user plus icon"></i>
                                                            { "Pending" }
                                                            <div class="detail">{ t.peers_pending }</div>
                                                        </div>
                                                    } else if panel == "Ratio" {
                                                        <div class="ui blue basic label">
                                                            <i class="exchange icon"></i>
                                                            { "Exchanged Ratio" }
                                                            <div class="detail">{ format!("{:.2}", t.seed_ratio) }</div>
                                                        </div>
                                                        <div class="ui blue basic label">
                                                            <i class="hourglass start icon"></i>
                                                            { "Started" }
                                                            <div class="detail">{ if !t.added_at.is_empty() { &t.added_at } else { "Unknown" } }</div>
                                                        </div>
                                                    } else if panel == "Downloaded" {
                                                        <div class="ui blue basic label">
                                                            <i class="cloud download icon"></i>
                                                            { "Downloaded Data" }
                                                            <div class="detail">{ format!("{} / {}", format_bytes(t.downloaded as u64), format_bytes(t.size as u64)) }</div>
                                                        </div>
                                                    }
                                                </div>
                                            </div>
                                        }
                                        if is_files_open {
                                            <div class="row">
                                                <div class="column">
                                                    <div class="ui fluid action input" style="margin-bottom: 5px;">
                                                        <input type="text" readonly=true value={t.magnet.clone()} />
                                                        <button class="ui teal right labeled icon button" onclick={
                                                            let magnet = t.magnet.clone();
                                                            Callback::from(move |_| {
                                                                copy_text_to_clipboard(&magnet);
                                                            })
                                                        }>
                                                            <i class="copy icon"></i>
                                                            { "Copy Magnet" }
                                                        </button>
                                                    </div>
                                                    <table class="ui unstackable compact striped downloads table">
                                                        <thead>
                                                            <tr><th>{ "File" }</th><th>{ "Size" }</th></tr>
                                                        </thead>
                                                        <tbody>
                                                            { for t.files.iter().map(|f| html! {
                                                                <tr>
                                                                    <td>{ f.get("Path").and_then(|v| v.as_str()).unwrap_or("?") }</td>
                                                                    <td>{ format_bytes(f.get("Size").and_then(|v| v.as_u64()).unwrap_or(0)) }</td>
                                                                </tr>
                                                            }) }
                                                        </tbody>
                                                    </table>
                                                </div>
                                            </div>
                                        }
                                    </div>
                                }
                            }) }
                        </div>
                    }

                    // Downloads Section
                    <div class="ui grid section-header" onclick={let downloads_expanded = downloads_expanded.clone(); let refresh_files = refresh_files.clone(); Callback::from(move |_| {
                        let new_state = !*downloads_expanded;
                        downloads_expanded.set(new_state);
                        if new_state { refresh_files.emit(()); }
                    })}>
                        <div class="column">
                            <i class={classes!("square", "outline", "icon", if *downloads_expanded { "minus" } else { "plus" })}></i>
                            <span class="ui header">
                                { " Downloads " }
                                <span class="ui label">
                                    <i class="hdd icon"></i>
                                    { format!(" {} free", format_bytes(gs.stats.system.disk_free)) }
                                </span>
                            </span>
                        </div>
                    </div>
                    if *downloads_expanded {
                        <div class="ui raised segment">
                            <div class="ui list">
                                if let Some(root) = &*files {
                                    if let Some(children) = root.get("Children").and_then(|v| v.as_array()) {
                                        { for children.iter().map(|child| html! {
                                            <FileNode node={child.clone()} path={""} on_delete={on_delete_file.clone()} />
                                        }) }
                                    } else {
                                        <div class="ui message"><p>{ "Download directory is empty." }</p></div>
                                    }
                                } else {
                                    <div class="ui active centered inline loader"></div>
                                }
                            </div>
                        </div>
                    }
                </div>
            }

            <footer>
                <div>
                    <span onclick={let show_engine_status = show_engine_status.clone(); move |_| show_engine_status.set(!*show_engine_status)} style="cursor: pointer;">
                        { "Cloud Torrent-rs made by OctopusTakopi" }
                        { " | " }
                        <span class="ui teal text">{ "Debug" }</span>
                    </span>
                </div>
            </footer>

            if *show_engine_status {
                if let Some(gs) = &*state {
                    <div class="ui attached mini message">
                        <i onclick={let show_engine_status = show_engine_status.clone(); move |_| show_engine_status.set(false)} class="close icon"></i>
                        <div class="header">{ "System Info" }</div>
                        <ul class="ui list">
                            <li>{ format!("Version: v{}", gs.stats.system.version) }</li>
                            <li>{ format!("Active Tasks: {}", gs.stats.system.active_tasks) }</li>
                            <li>{ format!("AppMemory (RES): {}", format_bytes(gs.stats.system.app_memory)) }</li>
                            <li>{ format!("CPU Usage: {:.1}%", gs.stats.system.cpu) }</li>
                            <li>{ format!("Sys Memory: {:.1}%", gs.stats.system.mem_used_percent) }</li>
                            <li>{ format!("Disk Used: {:.1}%", gs.stats.system.disk_used_percent) }</li>
                            <li>{ format!("DHT Nodes (IPv4): {}", gs.stats.system.dht.nodes4) }</li>
                            <li>{ format!("DHT Nodes (IPv6): {}", gs.stats.system.dht.nodes6) }</li>
                        </ul>
                    </div>
                }
            }
        </div>
    }
}

fn main() {
    wasm_logger::init(wasm_logger::Config::default());
    yew::Renderer::<App>::new().render();
}
