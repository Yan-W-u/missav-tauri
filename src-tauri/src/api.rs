//! HTTP API 层 + 静态资源服务。
//!
//! 复用现有前端（ui/index.html、ui/player.html）：它们的接口路径与 Python 版
//! 完全一致，所以只要这边实现同名路由，前端一行都不用改。
//!
//! 这是个极简 HTTP 服务（tokio 手写），不引入 axum/actix —— 目的是
//! 减小编译时间和二进制体积，同时也让手机端能通过局域网访问同一套接口。

use crate::fetch::{self, Fetcher};
use crate::hls;
use crate::site::{self, SiteClient};
use crate::tasks::{AppState, FavItem, Task};
use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct ApiServer {
    pub state: Arc<AppState>,
    pub ui_dir: PathBuf,
    pub port: u16,
}

impl ApiServer {
    /// 启动服务，绑定 0.0.0.0（手机同局域网可访问）。
    pub async fn start(state: Arc<AppState>, ui_dir: PathBuf) -> Result<u16> {
        let listener = TcpListener::bind(("0.0.0.0", 0)).await
            .map_err(|e| anyhow!("绑定端口失败: {e}"))?;
        let port = listener.local_addr()?.port();
        let srv = Arc::new(Self { state, ui_dir, port });

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, _)) => {
                        let s = srv.clone();
                        tokio::spawn(async move {
                            let _ = handle_conn(s, sock).await;
                        });
                    }
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                }
            }
        });
        Ok(port)
    }
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    #[allow(dead_code)]
    body: Vec<u8>,
}

async fn handle_conn(srv: Arc<ApiServer>, mut sock: TcpStream) -> Result<()> {
    // 读请求头
    let mut buf = vec![0u8; 64 * 1024];
    let mut filled = 0usize;
    let header_end;
    loop {
        if filled >= buf.len() {
            return Ok(());
        }
        let n = sock.read(&mut buf[filled..]).await?;
        if n == 0 {
            return Ok(());
        }
        filled += n;
        if let Some(p) = find_sub(&buf[..filled], b"\r\n\r\n") {
            header_end = p;
            break;
        }
    }

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let raw_path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    for l in lines {
        if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    // 读 body
    let mut body = buf[header_end + 4..filled].to_vec();
    while body.len() < content_length {
        let mut tmp = vec![0u8; 8192];
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    let (path, query) = split_path_query(&raw_path);
    let req = Request { method: method.clone(), path: path.clone(), query, body };

    // 路由
    let resp = route(&srv, req).await;
    let (status, ctype, payload) = resp;
    let mut out = Vec::new();
    out.extend_from_slice(
        format!("HTTP/1.1 {status} {}\r\n", reason(status)).as_bytes());
    out.extend_from_slice(format!("Content-Type: {ctype}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", payload.len()).as_bytes());
    out.extend_from_slice(b"Access-Control-Allow-Origin: *\r\n");
    out.extend_from_slice(b"Cache-Control: no-cache\r\n");
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(&payload);
    sock.write_all(&out).await?;
    sock.flush().await?;
    Ok(())
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK", 206 => "Partial Content", 400 => "Bad Request",
        403 => "Forbidden", 404 => "Not Found", 405 => "Method Not Allowed",
        500 => "Internal Server Error", 502 => "Bad Gateway", _ => "OK",
    }
}

async fn route(srv: &Arc<ApiServer>, req: Request) -> (u16, String, Vec<u8>) {
    let q = |k: &str| req.query.get(k).cloned().unwrap_or_default();

    // 静态资源
    if req.path == "/" || req.path == "/index.html" {
        return file_resp(&srv.ui_dir.join("index.html"), "text/html; charset=utf-8");
    }
    if req.path == "/player" || req.path == "/player.html" {
        return file_resp(&srv.ui_dir.join("player.html"), "text/html; charset=utf-8");
    }
    if req.path == "/manifest.webmanifest" {
        return file_resp(&srv.ui_dir.join("manifest.webmanifest"),
                         "application/manifest+json; charset=utf-8");
    }
    if req.path == "/sw.js" {
        return file_resp(&srv.ui_dir.join("sw.js"), "application/javascript; charset=utf-8");
    }
    if let Some(rest) = req.path.strip_prefix("/static/") {
        let p = srv.ui_dir.join(rest);
        let ct = match p.extension().and_then(|s| s.to_str()) {
            Some("js") => "application/javascript; charset=utf-8",
            Some("css") => "text/css; charset=utf-8",
            Some("png") => "image/png",
            Some("ico") => "image/x-icon",
            Some("json") => "application/json; charset=utf-8",
            _ => "application/octet-stream",
        };
        return file_resp(&p, ct);
    }

    // /stream/{token}/index.m3u8 与 /stream/{token}/p/{b64}
    if let Some(rest) = req.path.strip_prefix("/stream/") {
        return proxy_stream(srv, rest, q("referer")).await;
    }

    // ── API ──
    match req.path.as_str() {
        "/api/env" => {
            let proxy = srv.state.proxy.lock().unwrap().clone();
            let out = srv.state.default_out.lock().unwrap().to_string_lossy().to_string();
            return json_resp(200, json!({
                "ok": true,
                "proxy": proxy,
                "ffmpeg": hls::find_ffmpeg().map(|p| p.to_string_lossy().to_string()),
                "default_out": out,
                "version": "2.0.0",
                "instance": "tauri",
                "lan_port": srv.port,
            }));
        }
        "/api/connectivity" => {
            let proxy = srv.state.proxy.lock().unwrap().clone();
            let cdn_direct = check_cdn(None).await;
            let site_direct = check_site(None).await;
            return json_resp(200, json!({
                "ok": true, "proxy": proxy,
                "site_direct": site_direct, "site": site_direct,
                "cdn_direct": cdn_direct, "cdn": cdn_direct,
            }));
        }
        "/api/genres" => {
            let proxy = srv.state.proxy.lock().unwrap().clone();
            return match SiteClient::new(proxy, 20, 1) {
                Ok(c) => match c.genres().await {
                    Ok(g) => json_resp(200, json!({"ok": true, "genres": g})),
                    Err(e) => json_resp(200, json!({"ok": true, "genres": [],
                                                    "error": e.to_string()})),
                },
                Err(e) => json_resp(500, json!({"ok": false, "error": e.to_string()})),
            };
        }
        "/api/search" => {
            let kw = q("q");
            let page: usize = q("page").parse().unwrap_or(1);
            let proxy = srv.state.proxy.lock().unwrap().clone();
            return match SiteClient::new(proxy, 20, 1) {
                Ok(c) => match c.search_page(&kw, page).await {
                    Ok(p) => json_resp(200, json!({"ok": true, "items": p.items,
                                                  "page": p.page, "pages": p.pages,
                                                  "query": kw})),
                    Err(e) => json_resp(200, json!({"ok": false, "items": [],
                                                    "error": clean_err(&e.to_string())})),
                },
                Err(e) => json_resp(500, json!({"ok": false, "error": e.to_string()})),
            };
        }
        "/api/section" => {
            let sec = q("name");
            let page: usize = q("page").parse().unwrap_or(1);
            let proxy = srv.state.proxy.lock().unwrap().clone();
            return match SiteClient::new(proxy, 20, 1) {
                Ok(c) => match c.section_page(&sec, page).await {
                    Ok(p) => json_resp(200, json!({"ok": true, "items": p.items,
                                                  "page": p.page, "pages": p.pages,
                                                  "section": sec})),
                    Err(e) => json_resp(200, json!({"ok": false, "items": [],
                                                    "error": clean_err(&e.to_string())})),
                },
                Err(e) => json_resp(500, json!({"ok": false, "error": e.to_string()})),
            };
        }
        "/api/genre" => {
            let tag = q("tag");
            let page: usize = q("page").parse().unwrap_or(1);
            let proxy = srv.state.proxy.lock().unwrap().clone();
            return match SiteClient::new(proxy, 20, 1) {
                Ok(c) => match c.genre_page(&tag, page).await {
                    Ok(p) => json_resp(200, json!({"ok": true, "items": p.items,
                                                  "page": p.page, "pages": p.pages,
                                                  "tag": tag})),
                    Err(e) => json_resp(200, json!({"ok": false, "items": [],
                                                    "error": clean_err(&e.to_string())})),
                },
                Err(e) => json_resp(500, json!({"ok": false, "error": e.to_string()})),
            };
        }
        "/api/genre-multi" => {
            let raw = q("tags");
            let mode = { let m = q("mode"); if m.is_empty() { "union".into() } else { m } };
            let page: usize = q("page").parse().unwrap_or(1);
            let tags: Vec<String> = raw.split(',')
                .map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if tags.is_empty() {
                return json_resp(400, json!({"ok": false, "error": "缺少 tags"}));
            }
            if tags.len() > 8 {
                return json_resp(400, json!({"ok": false, "error": "一次最多选 8 个标签"}));
            }
            let proxy = srv.state.proxy.lock().unwrap().clone();
            return match SiteClient::new(proxy, 20, 1) {
                Ok(c) => match c.genre_multi(&tags, &mode, page).await {
                    Ok(p) => json_resp(200, json!({"ok": true, "items": p.items,
                                                  "tags": tags, "mode": mode,
                                                  "page": p.page, "pages": p.pages})),
                    Err(e) => json_resp(200, json!({"ok": false, "items": [],
                                                    "error": clean_err(&e.to_string())})),
                },
                Err(e) => json_resp(500, json!({"ok": false, "error": e.to_string()})),
            };
        }
        "/api/probe" => {
            let target = q("url");
            let proxy = srv.state.proxy.lock().unwrap().clone();
            return json_resp(200, probe_qualities(proxy, &target).await);
        }
        "/api/import/probe" => {
            let target = q("url");
            let proxy = srv.state.proxy.lock().unwrap().clone();
            return json_resp(200, import_probe(proxy, &target).await);
        }
        "/api/import/check" => {
            let raw = { let a = q("urls"); if a.is_empty() { q("url") } else { a } };
            let proxy = srv.state.proxy.lock().unwrap().clone();
            return json_resp(200, check_links(proxy, &raw).await);
        }
        "/api/stream" => {
            let target = q("url");
            let referer = { let r = q("referer"); if r.is_empty() {
                fetch::SITE_REFERER.to_string() } else { r } };
            if target.is_empty() {
                return json_resp(400, json!({"ok": false, "error": "缺少 url"}));
            }
            return match make_stream_token(srv, &target, &referer).await {
                Ok((token, play)) => json_resp(200, json!({
                    "ok": true, "token": token, "play_url": play })),
                Err(e) => json_resp(502, json!({"ok": false,
                                                "error": clean_err(&e.to_string())})),
            };
        }
        "/api/tasks" => {
            let g = srv.state.tasks.lock().unwrap();
            let mut v: Vec<Task> = g.values().cloned().collect();
            v.sort_by(|a, b| b.created.partial_cmp(&a.created).unwrap_or(std::cmp::Ordering::Equal));
            return json_resp(200, json!({"ok": true, "tasks": v}));
        }
        "/api/lan" => {
            let ips = lan_ips();
            let port = srv.port;
            let urls: Vec<String> = ips.iter().map(|ip| format!("http://{ip}:{port}/")).collect();
            return json_resp(200, json!({
                "ok": true, "enabled": true, "ips": ips, "port": port,
                "urls": urls, "qr": "", "fw_rule": true, "fw_admin": false,
            }));
        }
        "/api/history" => {
            srv.state.prune_missing();
            let h = srv.state.history.lock().unwrap().clone();
            return json_resp(200, json!({"ok": true, "items": h}));
        }
        "/api/favorites" => {
            let f = srv.state.favorites.lock().unwrap().clone();
            return json_resp(200, json!({"ok": true, "items": f,
                                         "count": f.len()}));
        }
        "/api/drives" => {
            return json_resp(200, json!({"ok": true, "drives": list_drives()}));
        }
        _ => {}
    }

    // POST 路由
    if req.method == "POST" {
        let payload: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
        match req.path.as_str() {
            "/api/download" => return start_task(srv, payload).await,
            "/api/task/cancel" => {
                let tid = payload.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let g = srv.state.tasks.lock().unwrap();
                if let Some(t) = g.get(tid) {
                    if let Some(c) = &t.cancel {
                        c.store(true, Ordering::Relaxed);
                    }
                }
                return json_resp(200, json!({"ok": true}));
            }
            "/api/task/delete" => {
                let tid = payload.get("id").and_then(|v| v.as_str()).unwrap_or("");
                srv.state.tasks.lock().unwrap().remove(tid);
                return json_resp(200, json!({"ok": true}));
            }
            "/api/favorite/toggle" => {
                let slug = payload.get("slug").and_then(|v| v.as_str()).unwrap_or("");
                if slug.is_empty() {
                    return json_resp(400, json!({"ok": false, "error": "缺少 slug"}));
                }
                let item = FavItem {
                    slug: slug.to_string(),
                    title: payload.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    url: payload.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    cover: payload.get("cover").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    censor: payload.get("censor").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    added: crate::tasks::now_pub(),
                };
                let on = srv.state.toggle_favorite(item);
                return json_resp(200, json!({"ok": true, "favorited": on}));
            }
            "/api/history/delete" => {
                let slug = payload.get("slug").and_then(|v| v.as_str()).unwrap_or("");
                {
                    let mut h = srv.state.history.lock().unwrap();
                    h.retain(|x| x.slug != slug);
                }
                srv.state.save_history();
                return json_resp(200, json!({"ok": true}));
            }
            "/api/history/clear" => {
                srv.state.history.lock().unwrap().clear();
                srv.state.save_history();
                return json_resp(200, json!({"ok": true}));
            }
            _ => {}
        }
    }

    json_resp(404, json!({"ok": false, "error": "未知接口"}))
}

fn clean_err(s: &str) -> String {
    // 把冗长的 reqwest 报错压成一句
    if s.contains("连接超时") { return "站点连接超时（检查代理是否开启）".into(); }
    if s.contains("连接失败") { return "站点连接失败（DNS 或网络不通）".into(); }
    if s.contains("TLS") { return "TLS 握手失败（可能被 SNI 阻断，或代理证书有问题）".into(); }
    s.chars().take(300).collect()
}

fn json_resp(code: u16, v: Value) -> (u16, String, Vec<u8>) {
    (code, "application/json; charset=utf-8".into(),
     serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec()))
}

fn file_resp(p: &std::path::Path, ct: &str) -> (u16, String, Vec<u8>) {
    match std::fs::read(p) {
        Ok(b) => (200, ct.to_string(), b),
        Err(_) => (404, "text/plain; charset=utf-8".into(), b"not found".to_vec()),
    }
}

async fn start_task(srv: &Arc<ApiServer>, payload: Value) -> (u16, String, Vec<u8>) {
    let url = payload.get("url").or_else(|| payload.get("id"))
        .and_then(|v| v.as_str()).unwrap_or("").to_string();
    if url.is_empty() {
        return json_resp(400, json!({"ok": false, "error": "缺少 id/url"}));
    }
    let ident = if url.starts_with("http") {
        url.clone()
    } else {
        format!("{}/cn/{}", fetch::SITE, site::normalize_id(&url))
    };
    let out_dir = payload.get("out_dir").and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| srv.state.default_out.lock().unwrap().to_string_lossy().to_string());
    let quality = payload.get("quality").and_then(|v| v.as_str())
        .unwrap_or("best").to_string();
    let threads = payload.get("threads").and_then(|v| v.as_u64())
        .unwrap_or(16) as usize;

    let tid = srv.state.next_id();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = Task {
        id: tid.clone(),
        url: ident.clone(),
        out_dir: out_dir.clone(),
        quality: quality.clone(),
        threads,
        slug: payload.get("slug").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        title: payload.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        censor: payload.get("censor").and_then(|v| v.as_str()).map(|s| s.to_string()),
        cover: payload.get("cover").and_then(|v| v.as_str()).map(|s| s.to_string()),
        status: "queued".into(),
        progress: 0.0,
        done_seg: 0,
        total_seg: 0,
        bytes: 0,
        speed_bps: 0.0,
        eta_secs: 0.0,
        message: "排队中".into(),
        file: None,
        error: None,
        created: crate::tasks::now_pub(),
        cancel: Some(cancel.clone()),
    };
    srv.state.tasks.lock().unwrap().insert(tid.clone(), task.clone());

    let st = srv.state.clone();
    let tid2 = tid.clone();
    tokio::spawn(async move {
        crate::tasks::run_download(
            st, tid2, ident, PathBuf::from(out_dir), quality, threads, cancel,
        ).await;
    });

    json_resp(200, json!({"ok": true, "task": task}))
}

async fn probe_qualities(proxy: Option<String>, target: &str) -> Value {
    let url = if target.starts_with("http") {
        target.to_string()
    } else {
        format!("{}/cn/{}", fetch::SITE, site::normalize_id(target))
    };
    let f = match Fetcher::new(proxy, 45, 3) {
        Ok(f) => f,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    let (m3u8, text) = match site::resolve_stream(&f, &url, None).await {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": clean_err(&e.to_string()),
                                "info": {"url": url, "playlist": ""}}),
    };
    let detail = site::fetch_detail(&f, &url, None).await
        .unwrap_or_else(|_| site::Detail { url: url.clone(), ..Default::default() });

    let variants = hls::parse_master(&text, &m3u8);
    let mut qs: Vec<Value> = variants.iter()
        .filter(|v| v.height > 0)
        .map(|v| json!({
            "label": format!("{}p", v.height),
            "height": v.height, "width": v.width,
            "bandwidth": v.bandwidth,
        }))
        .collect();
    if qs.is_empty() {
        qs.push(json!({"label": "原画", "height": 0, "width": 0, "bandwidth": 0}));
    }
    json!({
        "ok": true,
        "info": detail,
        "qualities": qs,
        "variants": variants,
        "playlist": m3u8,
        "stream_id": site::playlist_uuid(&m3u8),
    })
}

async fn import_probe(proxy: Option<String>, target: &str) -> Value {
    if target.is_empty() {
        return json!({"ok": false, "error": "缺少 url", "info": {}});
    }
    let f = match Fetcher::new(proxy, 45, 3) {
        Ok(f) => f,
        Err(e) => return json!({"ok": false, "error": e.to_string(), "info": {}}),
    };
    match site::resolve_stream(&f, target, None).await {
        Ok((m3u8, text)) => {
            let variants = hls::parse_master(&text, &m3u8);
            let mut qs: Vec<Value> = variants.iter().filter(|v| v.height > 0)
                .map(|v| json!({"label": format!("{}p", v.height), "height": v.height,
                                "width": v.width, "bandwidth": v.bandwidth})).collect();
            if qs.is_empty() {
                qs.push(json!({"label": "原画", "height": 0, "width": 0, "bandwidth": 0}));
            }
            json!({
                "ok": true, "external": true,
                "info": {"url": target, "id": "外部视频", "title": "",
                         "full_title": "", "playlist": m3u8,
                         "external": true, "censor": null},
                "qualities": qs,
                "playlist": m3u8,
                "stream_id": site::playlist_uuid(&m3u8),
            })
        }
        Err(e) => json!({"ok": false, "error": clean_err(&e.to_string()),
                         "info": {"url": target, "playlist": "", "external": true}}),
    }
}

/// 外部链接连通性检测（三级判定）
async fn check_links(proxy: Option<String>, raw: &str) -> Value {
    let lines: Vec<String> = raw.replace("\r\n", "\n").replace('\r', "\n")
        .split('\n').map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()).collect();
    if lines.is_empty() {
        return json!({"ok": false, "error": "没有识别到可用的网址", "items": []});
    }

    let f = match Fetcher::new(proxy.clone(), 15, 1) {
        Ok(f) => f,
        Err(e) => return json!({"ok": false, "error": e.to_string(), "items": []}),
    };
    // 代理不通就退直连：不然会把可用链接误判为失败
    let f = if proxy.is_some() {
        let first = normalize_input(&lines[0]);
        match first {
            Some(u) if f.probe(&u, Some(&u)).await.is_err() => {
                Fetcher::new(None, 15, 1).unwrap_or(f)
            }
            _ => f,
        }
    } else { f };

    let mut items: Vec<Value> = Vec::new();
    for orig in lines.iter().take(50) {
        let t0 = std::time::Instant::now();
        let Some(link) = normalize_input(orig) else {
            items.push(json!({"url": orig, "host": "", "status": "failed",
                              "code": null, "title": "", "stream": "", "ms": 0,
                              "error": "不是有效的网址（含空格或格式不像域名），请以 http(s):// 开头或写成 example.com 形式",
                              "resolved": ""}));
            continue;
        };
        let host = url::Url::parse(&link).ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_default();

        match f.probe(&link, Some(&link)).await {
            Ok((code, _)) if (200..400).contains(&code) => {
                // 能打开，再看有没有播放源
                match site::resolve_stream(&f, &link, None).await {
                    Ok((stream, _)) => items.push(json!({
                        "url": link, "host": host, "status": "has_stream",
                        "code": code, "title": "", "stream": stream,
                        "ms": t0.elapsed().as_millis() as u64, "error": "",
                        "resolved": link})),
                    Err(e) => items.push(json!({
                        "url": link, "host": host, "status": "reachable",
                        "code": code, "title": "", "stream": "",
                        "ms": t0.elapsed().as_millis() as u64,
                        "error": clean_err(&e.to_string()), "resolved": link})),
                }
            }
            Ok((code, _)) => items.push(json!({
                "url": link, "host": host, "status": "failed", "code": code,
                "title": "", "stream": "", "ms": t0.elapsed().as_millis() as u64,
                "error": format!("HTTP {code}（页面打不开，可能是防盗链或需要登录）"),
                "resolved": link})),
            Err(e) => {
                // 补全过 https 的裸域名：https 失败回退 http 再试
                let mut fell = false;
                if !orig.starts_with("http") && link.starts_with("https://") {
                    let alt = format!("http://{}", &link[8..]);
                    if let Ok((c2, _)) = f.probe(&alt, Some(&alt)).await {
                        if (200..400).contains(&c2) {
                            items.push(json!({
                                "url": alt, "host": host, "status": "reachable",
                                "code": c2, "title": "", "stream": "",
                                "ms": t0.elapsed().as_millis() as u64,
                                "error": "页面能打开，但没找到 m3u8 播放地址",
                                "resolved": alt}));
                            fell = true;
                        }
                    }
                }
                if !fell {
                    items.push(json!({
                        "url": link, "host": host, "status": "failed", "code": null,
                        "title": "", "stream": "", "ms": t0.elapsed().as_millis() as u64,
                        "error": clean_err(&e.to_string()), "resolved": link}));
                }
            }
        }
    }

    let playable = items.iter().filter(|i| i["status"] == "has_stream").count();
    let reachable = items.iter().filter(|i| i["status"] == "reachable").count();
    let total = items.len();
    json!({"ok": true, "items": items, "total": total, "playable": playable,
           "reachable": reachable, "failed": total - playable - reachable})
}

/// 裸域名补协议（与 Python 版 urlparse_util 同一套规则）
pub fn normalize_input(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() { return None; }
    if s.starts_with("http://") || s.starts_with("https://") {
        return Some(s.to_string());
    }
    if s.contains(char::is_whitespace) { return None; }
    // 取 host 部分判断
    let hostpart = s.split(['/', '?', '#']).next().unwrap_or("");
    if hostpart.is_empty() { return None; }
    let host = hostpart.split(':').next().unwrap_or("");
    if host == "localhost" {
        return Some(format!("https://{s}"));
    }
    if host.split('.').count() < 2 { return None; }
    let tld = host.rsplit('.').next().unwrap_or("");
    if tld.len() < 2 || !tld.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    if !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
        return None;
    }
    Some(format!("https://{s}"))
}

async fn make_stream_token(srv: &Arc<ApiServer>, target: &str, referer: &str)
    -> Result<(String, String)>
{
    use md5::{Digest, Md5};
    // 若给的是普通页面，先解析出 m3u8
    let (url, referer) = if target.split('?').next().unwrap_or("")
        .to_lowercase().ends_with(".m3u8")
    {
        (target.to_string(), referer.to_string())
    } else {
        let proxy = srv.state.proxy.lock().unwrap().clone();
        let f = Fetcher::new(proxy, 45, 3)?;
        let (m3u8, _) = site::resolve_stream(&f, target, Some(referer)).await?;
        (m3u8, target.to_string())
    };

    // 返回前校验一次，避免前端拿到"成功"却在播放器里莫名黑屏
    let proxy = srv.state.proxy.lock().unwrap().clone();
    let f = Fetcher::new(proxy, 30, 2)?;
    let head = f.get_text(&url, Some(&referer)).await?;
    if !head.trim_start().to_uppercase().starts_with("#EXTM3U") {
        return Err(anyhow!("该地址返回的不是 m3u8（可能被拦截或已失效）"));
    }

    let mut h = Md5::new();
    h.update(format!("{url}|{referer}").as_bytes());
    let token: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string();

    srv.state.streams.lock().unwrap().insert(
        token.clone(),
        crate::tasks::StreamEntry { url, referer },
    );
    Ok((token.clone(), format!("/stream/{token}/index.m3u8")))
}

/// 反向代理 m3u8 与分片：由服务端补 Referer，浏览器端只连本地
async fn proxy_stream(srv: &Arc<ApiServer>, rest: &str, _ref_q: String)
    -> (u16, String, Vec<u8>)
{
    let parts: Vec<&str> = rest.trim_matches('/').split('/').collect();
    if parts.len() < 2 {
        return json_resp(404, json!({"ok": false, "error": "bad stream path"}));
    }
    let token = parts[0];
    let ent = srv.state.streams.lock().unwrap().get(token).cloned();
    let Some(ent) = ent else {
        return json_resp(404, json!({"ok": false, "error": "stream token 不存在"}));
    };

    let target = if parts[1] == "index.m3u8" {
        ent.url.clone()
    } else if parts[1] == "p" && parts.len() >= 3 {
        let enc = parts[2];
        let mut s = enc.to_string();
        while s.len() % 4 != 0 { s.push('='); }
        match base64::engine::general_purpose::URL_SAFE.decode(s) {
            Ok(b) => String::from_utf8_lossy(&b).to_string(),
            Err(_) => return json_resp(400, json!({"ok": false, "error": "bad target"})),
        }
    } else {
        return json_resp(404, json!({"ok": false, "error": "bad path"}));
    };

    let proxy = srv.state.proxy.lock().unwrap().clone();
    let f = match Fetcher::new(proxy, 45, 3) {
        Ok(f) => f,
        Err(e) => return json_resp(502, json!({"ok": false, "error": e.to_string()})),
    };
    let bytes = match f.get_bytes(&target, Some(&ent.referer), 60).await {
        Ok(b) => b,
        Err(e) => return json_resp(502, json!({"ok": false, "error": clean_err(&e.to_string())})),
    };

    let is_m3u8 = target.split('?').next().unwrap_or("").to_lowercase().ends_with(".m3u8")
        || bytes.starts_with(b"#EXTM3U");
    if !is_m3u8 {
        // 分片原样转发（可能是伪装成 .jpeg 的 TS）
        return (200, "application/octet-stream".into(), bytes);
    }

    let text = String::from_utf8_lossy(&bytes).to_string();
    let rewritten = rewrite_m3u8(&text, &target, token);
    (200, "application/vnd.apple.mpegurl".into(), rewritten.into_bytes())
}

/// 把 m3u8 里的所有远程地址改写成本地代理地址
fn rewrite_m3u8(text: &str, base: &str, token: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            out.push('\n');
            continue;
        }
        if t.starts_with('#') {
            // 只改写 URI="..." 里的地址（EXT-X-KEY / EXT-X-MAP）
            if let Some(start) = t.find("URI=\"") {
                if let Some(end) = t[start + 5..].find('"') {
                    let uri = &t[start + 5..start + 5 + end];
                    if let Some(abs) = hls::resolve_url(base, uri) {
                        let enc = base64::engine::general_purpose::URL_SAFE.encode(abs.as_bytes());
                        let newline = format!("{}URI=\"/stream/{}/p/{}\"{}",
                            &t[..start], token, enc, &t[start + 5 + end + 1..]);
                        out.push_str(&newline);
                        out.push('\n');
                        continue;
                    }
                }
            }
            out.push_str(t);
            out.push('\n');
            continue;
        }
        // 地址行
        if let Some(abs) = hls::resolve_url(base, t) {
            let enc = base64::engine::general_purpose::URL_SAFE.encode(abs.as_bytes());
            out.push_str(&format!("/stream/{token}/p/{enc}"));
        } else {
            out.push_str(t);
        }
        out.push('\n');
    }
    out
}

async fn check_site(proxy: Option<String>) -> bool {
    check_site_pub(proxy).await
}

async fn check_cdn(proxy: Option<String>) -> bool {
    check_cdn_pub(proxy).await
}

/// 站点可达性（公开版，移动端复用）
pub async fn check_site_pub(proxy: Option<String>) -> bool {
    let f = match Fetcher::new(proxy, 8, 1) {
        Ok(f) => f,
        Err(_) => return false,
    };
    f.probe(&format!("{}/cn", fetch::SITE), None).await
        .map(|(c, _)| (200..400).contains(&c)).unwrap_or(false)
}

/// CDN 可达性（公开版，移动端复用）
pub async fn check_cdn_pub(proxy: Option<String>) -> bool {
    let f = match Fetcher::new(proxy, 8, 1) {
        Ok(f) => f,
        Err(_) => return false,
    };
    f.probe("https://fourhoi.com/ssis-001/cover-t.jpg", None).await
        .map(|(c, _)| (200..400).contains(&c)).unwrap_or(false)
}

pub fn lan_ips() -> Vec<String> {
    let mut out = Vec::new();
    // 用 UDP 连接选出默认出口网卡（不会真发包）
    if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if s.connect("223.5.5.5:53").is_ok() {
            if let Ok(addr) = s.local_addr() {
                let ip = addr.ip().to_string();
                if !ip.starts_with("127.") {
                    out.push(ip);
                }
            }
        }
    }
    out
}

fn list_drives() -> Vec<Value> {
    let mut out = Vec::new();
    #[cfg(windows)]
    {
        for ch in b'A'..=b'Z' {
            let p = format!("{}:\\", ch as char);
            if std::path::Path::new(&p).exists() {
                out.push(json!({"path": p, "label": format!("{}:", ch as char),
                                "free": 0u64, "total": 0u64, "type": "local",
                                "writable": true}));
            }
        }
    }
    #[cfg(not(windows))]
    {
        out.push(json!({"path": "/", "label": "/", "free": 0u64,
                        "total": 0u64, "type": "local", "writable": true}));
    }
    out
}

fn split_path_query(raw: &str) -> (String, HashMap<String, String>) {
    let mut q = HashMap::new();
    let (path, qs) = match raw.split_once('?') {
        Some((p, qs)) => (p.to_string(), qs),
        None => (raw.to_string(), ""),
    };
    for pair in qs.split('&') {
        if pair.is_empty() { continue; }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = percent_encoding::percent_decode_str(k).decode_utf8_lossy().to_string();
        let v = percent_encoding::percent_decode_str(v).decode_utf8_lossy().to_string();
        q.insert(k, v);
    }
    (path, q)
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

// ── 供移动端（lib.rs 的命令）复用的公开包装 ──

/// 外部链接连通性检测（公开版）
pub async fn check_links_pub(proxy: Option<String>, raw: &str) -> Value {
    check_links(proxy, raw).await
}

/// 生成播放令牌（公开版）：返回给前端的 play_url
pub async fn stream_url_pub(state: &Arc<AppState>, target: &str, referer: &str)
    -> Value
{
    if target.is_empty() {
        return json!({"ok": false, "error": "缺少 url"});
    }
    let ref_s = if referer.is_empty() { fetch::SITE_REFERER } else { referer };
    // 移动端没有本地 HTTP 服务，直接把 m3u8 内容取回来交给前端不合适
    // （还需改写分片地址）。这里返回解析后的 m3u8 地址，前端可用它
    // 直接喂给原生播放器（Android 的 ExoPlayer 支持 HLS）。
    let proxy = state.proxy.lock().unwrap().clone();
    let f = match Fetcher::new(proxy, 45, 3) {
        Ok(f) => f,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    if target.split('?').next().unwrap_or("").to_lowercase().ends_with(".m3u8") {
        match f.get_text(target, Some(ref_s)).await {
            Ok(_) => json!({"ok": true, "play_url": target, "direct": true}),
            Err(e) => json!({"ok": false, "error": clean_err(&e.to_string())}),
        }
    } else {
        match site::resolve_stream(&f, target, Some(ref_s)).await {
            Ok((m3u8, _)) => json!({"ok": true, "play_url": m3u8, "direct": true}),
            Err(e) => json!({"ok": false, "error": clean_err(&e.to_string())}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_input_rules() {
        assert_eq!(normalize_input("iav24.com").as_deref(), Some("https://iav24.com"));
        assert_eq!(normalize_input("iav24.com/path").as_deref(),
                   Some("https://iav24.com/path"));
        assert_eq!(normalize_input("https://a.com/x?y=1").as_deref(),
                   Some("https://a.com/x?y=1"));
        assert!(normalize_input("hello world").is_none());
        assert!(normalize_input("SSIS-001").is_none());
        assert!(normalize_input("1.5").is_none());
    }

    #[test]
    fn rewrite_m3u8_rewrites_segments() {
        let txt = "#EXTM3U\n#EXTINF:4.0,\nseg0.ts\n";
        let out = rewrite_m3u8(txt, "https://surrit.com/u/x/video.m3u8", "tok");
        assert!(out.contains("/stream/tok/p/"), "未改写: {out}");
        assert!(!out.contains("surrit.com/u/x/seg0.ts"));
    }

    #[test]
    fn rewrite_m3u8_rewrites_key_uri() {
        let txt = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\ns.ts\n";
        let out = rewrite_m3u8(txt, "https://a.com/x/video.m3u8", "tk");
        assert!(out.contains("URI=\"/stream/tk/p/"), "密钥地址未改写: {out}");
    }

    #[test]
    fn split_path_query_works() {
        let (p, q) = split_path_query("/api/search?q=abc&page=2");
        assert_eq!(p, "/api/search");
        assert_eq!(q.get("q").unwrap(), "abc");
        assert_eq!(q.get("page").unwrap(), "2");
    }
}
