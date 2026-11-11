//! 库入口：桌面端与移动端共用。
//!
//! 架构：
//!   * **桌面端**：后台起一个 HTTP 服务（绑定 0.0.0.0，手机同局域网可访问），
//!     Tauri 窗口加载 http://127.0.0.1:PORT。
//!     为什么用 HTTP 而不是 Tauri IPC：现有前端是用 fetch('/api/...') 写的，
//!     走 HTTP 就能一行不改地复用；同时手机端能访问同一套接口。
//!   * **移动端**：Tauri 直接加载内置前端，通过 invoke 命令提供能力
//!     （手机上没有"本地 HTTP 服务"的位置，也没必要）。

mod api;
mod fetch;
mod hls;
mod site;
mod tasks;

use std::sync::Arc;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = tasks::AppState::new();

    // 后台探测代理（不阻塞启动）
    {
        let st = state.clone();
        tauri::async_runtime::spawn(async move {
            if let Some(p) = fetch::detect_proxy().await {
                *st.proxy.lock().unwrap() = Some(p);
                // 代理若是 TLS 中间人（根证书不在系统库），探测时会被标记，
                // 这里同步开启忽略证书，否则站点请求全报 UnknownIssuer。
                if fetch::PROXY_NEEDS_INSECURE.load(std::sync::atomic::Ordering::Relaxed) {
                    *st.insecure.lock().unwrap() = true;
                    fetch::GLOBAL_INSECURE.store(true, std::sync::atomic::Ordering::Relaxed);
                    println!("[i] 代理做了 TLS 解密，已自动忽略证书校验");
                }
            }
        });
    }

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            cmd_env, cmd_search, cmd_section, cmd_genre, cmd_genre_multi,
            cmd_genres, cmd_detail, cmd_download, cmd_tasks, cmd_task_cancel,
            cmd_history, cmd_favorites, cmd_fav_toggle, cmd_import_check,
            cmd_stream_url, cmd_lan, cmd_set_proxy, cmd_pick_folder,
            cmd_connectivity, cmd_proxy_status, cmd_proxy_detect,
        ]);

    // 桌面端：先起 HTTP 服务，再让窗口加载它
    #[cfg(desktop)]
    {
        let ui_dir = resolve_ui_dir();
        let port = tauri::async_runtime::block_on(async {
            api::ApiServer::start(state.clone(), ui_dir).await
        });
        let port = match port {
            Ok(p) => p,
            Err(e) => {
                eprintln!("启动本地服务失败: {e}");
                return;
            }
        };
        println!("MissAV 本地服务已启动: http://127.0.0.1:{port}");
        for ip in api::lan_ips() {
            println!("手机可访问: http://{ip}:{port}/");
        }
        // 桌面端让窗口直接加载本地服务地址，前端据此走 fetch 而不是 invoke
        let app = builder.build(tauri::generate_context!())
            .expect("构建 Tauri 应用失败");
        {
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let url = format!("http://127.0.0.1:{port}");
                let _ = w.navigate(url.parse().expect("本地地址无效"));
            }
        }
        let _ = app.run(|_app, _ev| {});
        return;
    }

    #[cfg(mobile)]
    {
        builder.run(tauri::generate_context!())
            .expect("Tauri 移动端启动失败");
    }
}

/// 静态资源目录：开发时用 ../ui，打包后在可执行文件旁
#[cfg(desktop)]
fn resolve_ui_dir() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("ui");
            if p.join("index.html").is_file() {
                return p;
            }
            let p2 = dir.join("../Resources/ui");
            if p2.join("index.html").is_file() {
                return p2;
            }
        }
    }
    let dev = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().map(|p| p.join("ui"))
        .unwrap_or_else(|| std::path::PathBuf::from("ui"));
    if dev.join("index.html").is_file() {
        return dev;
    }
    std::path::PathBuf::from("ui")
}

// ── 命令：桌面端(可选用) 与 移动端(必用) 共用 ──

use serde_json::{json, Value};
use tauri::State;

fn proxy_of(st: &tasks::AppState) -> Option<String> {
    st.proxy.lock().unwrap().clone()
}

#[tauri::command]
async fn cmd_env(st: State<'_, Arc<tasks::AppState>>) -> Result<Value, String> {
    Ok(json!({
        "ok": true,
        "proxy": proxy_of(&st),
        "ffmpeg": hls::find_ffmpeg().map(|p| p.to_string_lossy().to_string()),
        "default_out": st.default_out.lock().unwrap().to_string_lossy().to_string(),
        "version": "2.0.0",
        "instance": "tauri",
    }))
}

#[tauri::command]
async fn cmd_set_proxy(st: State<'_, Arc<tasks::AppState>>,
                       proxy: Option<String>) -> Result<Value, String> {
    *st.proxy.lock().unwrap() = proxy.filter(|s| !s.trim().is_empty());
    Ok(json!({"ok": true}))
}

#[tauri::command]
async fn cmd_search(st: State<'_, Arc<tasks::AppState>>, q: String, page: usize)
    -> Result<Value, String>
{
    let c = site::SiteClient::new(proxy_of(&st), 20, 1).map_err(|e| e.to_string())?;
    match c.search_page(&q, page).await {
        Ok(p) => Ok(json!({"ok": true, "items": p.items, "page": p.page,
                           "pages": p.pages, "query": q})),
        Err(e) => Ok(json!({"ok": false, "items": [], "error": e.to_string()})),
    }
}

#[tauri::command]
async fn cmd_section(st: State<'_, Arc<tasks::AppState>>, name: String, page: usize)
    -> Result<Value, String>
{
    let c = site::SiteClient::new(proxy_of(&st), 20, 1).map_err(|e| e.to_string())?;
    match c.section_page(&name, page).await {
        Ok(p) => Ok(json!({"ok": true, "items": p.items, "page": p.page,
                           "pages": p.pages, "section": name})),
        Err(e) => Ok(json!({"ok": false, "items": [], "error": e.to_string()})),
    }
}

#[tauri::command]
async fn cmd_genre(st: State<'_, Arc<tasks::AppState>>, tag: String, page: usize)
    -> Result<Value, String>
{
    let c = site::SiteClient::new(proxy_of(&st), 20, 1).map_err(|e| e.to_string())?;
    match c.genre_page(&tag, page).await {
        Ok(p) => Ok(json!({"ok": true, "items": p.items, "page": p.page,
                           "pages": p.pages, "tag": tag})),
        Err(e) => Ok(json!({"ok": false, "items": [], "error": e.to_string()})),
    }
}

#[tauri::command]
async fn cmd_genre_multi(st: State<'_, Arc<tasks::AppState>>, tags: Vec<String>,
                         mode: String, page: usize) -> Result<Value, String>
{
    if tags.is_empty() || tags.len() > 8 {
        return Ok(json!({"ok": false, "error": "标签数量应为 1~8 个"}));
    }
    let c = site::SiteClient::new(proxy_of(&st), 20, 1).map_err(|e| e.to_string())?;
    let m = if mode.is_empty() { "union".to_string() } else { mode };
    match c.genre_multi(&tags, &m, page).await {
        Ok(p) => Ok(json!({"ok": true, "items": p.items, "page": p.page,
                           "pages": p.pages, "tags": tags, "mode": m})),
        Err(e) => Ok(json!({"ok": false, "items": [], "error": e.to_string()})),
    }
}

#[tauri::command]
async fn cmd_genres(st: State<'_, Arc<tasks::AppState>>) -> Result<Value, String> {
    let c = site::SiteClient::new(proxy_of(&st), 20, 1).map_err(|e| e.to_string())?;
    match c.genres().await {
        Ok(g) => Ok(json!({"ok": true, "genres": g})),
        Err(e) => Ok(json!({"ok": true, "genres": [], "error": e.to_string()})),
    }
}

#[tauri::command]
async fn cmd_detail(st: State<'_, Arc<tasks::AppState>>, id: String)
    -> Result<Value, String>
{
    let c = site::SiteClient::new(proxy_of(&st), 45, 3).map_err(|e| e.to_string())?;
    // 前端 /api/probe 与 /api/detail 都走这里：
    // 它既要给详情，也要给清晰度列表，所以按详情 + 播放源解析来返回
    let target = if id.starts_with("http") {
        id.clone()
    } else {
        format!("{}/cn/{}", fetch::SITE, site::normalize_id(&id))
    };
    match site::resolve_stream(&c.f, &target, None).await {
        Ok((m3u8, text)) => {
            let detail = c.detail(&id).await.unwrap_or_else(|_| site::Detail {
                url: target.clone(), ..Default::default()
            });
            let variants = hls::parse_master(&text, &m3u8);
            let mut qs: Vec<Value> = variants.iter().filter(|v| v.height > 0)
                .map(|v| json!({"label": format!("{}p", v.height), "height": v.height,
                                "width": v.width, "bandwidth": v.bandwidth})).collect();
            if qs.is_empty() {
                qs.push(json!({"label": "原画", "height": 0, "width": 0,
                               "bandwidth": 0}));
            }
            Ok(json!({
                "ok": true, "info": detail, "qualities": qs,
                "variants": variants, "playlist": m3u8,
                "stream_id": site::playlist_uuid(&m3u8),
            }))
        }
        Err(e) => Ok(json!({"ok": false, "error": e.to_string(),
                            "info": {"url": target, "playlist": ""}})),
    }
}

#[tauri::command]
async fn cmd_download(st: State<'_, Arc<tasks::AppState>>, url: String,
                      out_dir: String, quality: String, threads: usize,
                      title: String, slug: String) -> Result<Value, String>
{
    let ident = if url.starts_with("http") {
        url.clone()
    } else {
        format!("{}/cn/{}", fetch::SITE, site::normalize_id(&url))
    };
    let out = if out_dir.trim().is_empty() {
        st.default_out.lock().unwrap().to_string_lossy().to_string()
    } else { out_dir };
    let tid = st.next_id();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = tasks::Task {
        id: tid.clone(), url: ident.clone(), out_dir: out.clone(),
        quality: quality.clone(), threads,
        slug, title, censor: None, cover: None,
        status: "queued".into(), progress: 0.0, done_seg: 0, total_seg: 0,
        bytes: 0, speed_bps: 0.0, eta_secs: 0.0, message: "排队中".into(),
        file: None, error: None, created: tasks::now_pub(),
        cancel: Some(cancel.clone()),
    };
    st.tasks.lock().unwrap().insert(tid.clone(), task.clone());
    let st2 = st.inner().clone();
    tokio::spawn(async move {
        tasks::run_download(st2, tid, ident, std::path::PathBuf::from(out),
                            quality, threads, cancel).await;
    });
    Ok(json!({"ok": true, "task": task}))
}

#[tauri::command]
async fn cmd_tasks(st: State<'_, Arc<tasks::AppState>>) -> Result<Value, String> {
    let g = st.tasks.lock().unwrap();
    let mut v: Vec<tasks::Task> = g.values().cloned().collect();
    v.sort_by(|a, b| b.created.partial_cmp(&a.created)
        .unwrap_or(std::cmp::Ordering::Equal));
    Ok(json!({"ok": true, "tasks": v}))
}

#[tauri::command]
async fn cmd_task_cancel(st: State<'_, Arc<tasks::AppState>>, id: String)
    -> Result<Value, String>
{
    let g = st.tasks.lock().unwrap();
    if let Some(t) = g.get(&id) {
        if let Some(c) = &t.cancel {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(json!({"ok": true}))
}

#[tauri::command]
async fn cmd_history(st: State<'_, Arc<tasks::AppState>>) -> Result<Value, String> {
    st.prune_missing();
    let h = st.history.lock().unwrap().clone();
    Ok(json!({"ok": true, "items": h}))
}

#[tauri::command]
async fn cmd_favorites(st: State<'_, Arc<tasks::AppState>>) -> Result<Value, String> {
    let f = st.favorites.lock().unwrap().clone();
    Ok(json!({"ok": true, "items": f, "count": f.len()}))
}

#[tauri::command]
async fn cmd_fav_toggle(st: State<'_, Arc<tasks::AppState>>, slug: String,
                        title: String, url: String, cover: String)
    -> Result<Value, String>
{
    let item = tasks::FavItem {
        slug, title, url, cover, censor: None, added: tasks::now_pub(),
    };
    let on = st.toggle_favorite(item);
    Ok(json!({"ok": true, "favorited": on}))
}

#[tauri::command]
async fn cmd_import_check(st: State<'_, Arc<tasks::AppState>>, urls: String)
    -> Result<Value, String>
{
    Ok(api::check_links_pub(proxy_of(&st), &urls).await)
}

#[tauri::command]
async fn cmd_stream_url(st: State<'_, Arc<tasks::AppState>>, url: String,
                        referer: String) -> Result<Value, String>
{
    Ok(api::stream_url_pub(&st, &url, &referer).await)
}

#[tauri::command]
async fn cmd_lan() -> Result<Value, String> {
    Ok(json!({"ok": true, "ips": api::lan_ips()}))
}

/// 连通性检测：站点 / CDN 各自可达与否。
/// 移动端没有本地 HTTP 服务，前端改用这个命令拿同样的结论。
#[tauri::command]
async fn cmd_connectivity(st: State<'_, Arc<tasks::AppState>>)
    -> Result<Value, String>
{
    let proxy = proxy_of(&st);
    let site = api::check_site_pub(proxy.clone()).await;
    let cdn = api::check_cdn_pub(proxy.clone()).await;
    Ok(json!({
        "ok": true,
        "proxy": proxy,
        "site": site, "site_direct": site,
        "cdn": cdn, "cdn_direct": cdn,
        "insecure": *st.insecure.lock().unwrap(),
    }))
}

/// 代理状态（前端 checkProxy() 用这个）
#[tauri::command]
async fn cmd_proxy_status(st: State<'_, Arc<tasks::AppState>>)
    -> Result<Value, String>
{
    let proxy = proxy_of(&st);
    // alive = 真的能连外网，不是只看有没有配置
    let alive = match proxy.clone() {
        Some(p) => {
            match fetch::Fetcher::with_options(Some(p), 10, 1,
                     fetch::GLOBAL_INSECURE.load(std::sync::atomic::Ordering::Relaxed)) {
                Ok(f) => f.probe("https://www.baidu.com/", None).await
                    .map(|(c, _)| (200..400).contains(&c)).unwrap_or(false),
                Err(_) => false,
            }
        }
        None => false,
    };
    Ok(json!({"ok": true, "proxy": proxy, "alive": alive}))
}

/// 手动设置/重新探测代理
#[tauri::command]
async fn cmd_proxy_detect(st: State<'_, Arc<tasks::AppState>>)
    -> Result<Value, String>
{
    // 清掉缓存，强制重探
    fetch::reset_proxy_cache().await;
    let found = fetch::detect_proxy().await;
    *st.proxy.lock().unwrap() = found.clone();
    if fetch::PROXY_NEEDS_INSECURE.load(std::sync::atomic::Ordering::Relaxed) {
        *st.insecure.lock().unwrap() = true;
        fetch::GLOBAL_INSECURE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(json!({"ok": true, "proxy": found}))
}

/// 选目录：桌面用系统文件夹选择器；移动端没有这个概念（Android 是 SAF），
/// 直接返回默认下载目录，前端据此提示即可。
#[tauri::command]
async fn cmd_pick_folder(app: tauri::AppHandle,
                         st: State<'_, Arc<tasks::AppState>>) -> Result<Value, String> {
    #[cfg(desktop)]
    {
        use tauri_plugin_dialog::DialogExt;
        let picked = app.dialog().file().blocking_pick_folder();
        return Ok(json!({
            "ok": picked.is_some(),
            "path": picked.map(|p| p.to_string()).unwrap_or_default()
        }));
    }
    #[cfg(mobile)]
    {
        let _ = app;
        Ok(json!({
            "ok": true,
            "path": st.default_out.lock().unwrap().to_string_lossy().to_string(),
            "note": "手机端固定保存到应用私有目录，不需要选择"
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::site;

    #[test]
    fn normalize_reused() {
        assert_eq!(site::normalize_id("SSIS-001"), "ssis-001");
    }
}
