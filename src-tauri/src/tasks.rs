//! 下载任务管理：队列、进度、取消、历史。
//!
//! 对应原 Python 版 `app.py` 的 `State` + `run_task`，以及 `history.py`。

use crate::fetch::Fetcher;
use crate::hls::{self, KeyStore, Progress};
use crate::site;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// 公开的时间戳（给 lib.rs 的移动端命令用）
pub fn now_pub() -> f64 { now() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub url: String,
    pub out_dir: String,
    pub quality: String,
    pub threads: usize,
    pub slug: String,
    pub title: String,
    pub censor: Option<String>,
    pub cover: Option<String>,
    /// queued | running | concat | remux | done | error | canceled
    pub status: String,
    pub progress: f64,
    pub done_seg: usize,
    pub total_seg: usize,
    pub bytes: u64,
    pub speed_bps: f64,
    pub eta_secs: f64,
    pub message: String,
    pub file: Option<String>,
    pub error: Option<String>,
    pub created: f64,
    #[serde(skip)]
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub slug: String,
    pub title: String,
    pub url: String,
    pub quality: String,
    pub file: String,
    pub size: u64,
    pub censor: Option<String>,
    pub cover: String,
    pub status: String,
    pub finished: f64,
    pub missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FavItem {
    pub slug: String,
    pub title: String,
    pub url: String,
    pub cover: String,
    pub censor: Option<String>,
    pub added: f64,
}

/// 全局状态
pub struct AppState {
    pub proxy: Mutex<Option<String>>,
    pub insecure: Mutex<bool>,
    pub tasks: Mutex<HashMap<String, Task>>,
    pub seq: AtomicU64,
    pub default_out: Mutex<PathBuf>,
    pub history: Mutex<Vec<HistoryItem>>,
    pub favorites: Mutex<Vec<FavItem>>,
    pub streams: Mutex<HashMap<String, StreamEntry>>,
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct StreamEntry {
    pub url: String,
    pub referer: String,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("MissAV下载器");
        let _ = std::fs::create_dir_all(&data_dir);

        let default_out = dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("MissAV");
        let _ = std::fs::create_dir_all(&default_out);

        let st = Arc::new(Self {
            proxy: Mutex::new(None),
            insecure: Mutex::new(false),
            tasks: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
            default_out: Mutex::new(default_out),
            history: Mutex::new(Vec::new()),
            favorites: Mutex::new(Vec::new()),
            streams: Mutex::new(HashMap::new()),
            data_dir: data_dir.clone(),
        });
        st.load_history();
        st.load_favorites();
        st
    }

    fn hist_path(&self) -> PathBuf { self.data_dir.join("history.json") }
    fn fav_path(&self) -> PathBuf { self.data_dir.join("favorites.json") }

    fn load_history(&self) {
        if let Ok(s) = std::fs::read_to_string(self.hist_path()) {
            if let Ok(v) = serde_json::from_str::<Vec<HistoryItem>>(&s) {
                *self.history.lock().unwrap() = v;
            }
        }
    }

    fn load_favorites(&self) {
        if let Ok(s) = std::fs::read_to_string(self.fav_path()) {
            if let Ok(v) = serde_json::from_str::<Vec<FavItem>>(&s) {
                *self.favorites.lock().unwrap() = v;
            }
        }
    }

    pub fn save_history(&self) {
        let v = self.history.lock().unwrap().clone();
        // 原子写：先写临时文件再改名，避免断电把文件写坏
        let tmp = self.hist_path().with_extension("json.tmp");
        if let Ok(s) = serde_json::to_string_pretty(&v) {
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, self.hist_path());
            }
        }
    }

    pub fn save_favorites(&self) {
        let v = self.favorites.lock().unwrap().clone();
        let tmp = self.fav_path().with_extension("json.tmp");
        if let Ok(s) = serde_json::to_string_pretty(&v) {
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, self.fav_path());
            }
        }
    }

    pub fn next_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        format!("t{n}_{}", (now() * 1000.0) as u64)
    }

    pub fn record_history(&self, it: HistoryItem) {
        {
            let mut h = self.history.lock().unwrap();
            h.retain(|x| !(x.slug == it.slug && x.quality == it.quality));
            h.insert(0, it);
            if h.len() > 500 {
                h.truncate(500);
            }
        }
        self.save_history();
    }

    pub fn prune_missing(&self) {
        let mut changed = false;
        {
            let mut h = self.history.lock().unwrap();
            for it in h.iter_mut() {
                let missing = !it.file.is_empty() && !Path::new(&it.file).exists();
                if it.missing != missing {
                    it.missing = missing;
                    changed = true;
                }
            }
        }
        if changed {
            self.save_history();
        }
    }

    pub fn toggle_favorite(&self, item: FavItem) -> bool {
        let on;
        {
            let mut f = self.favorites.lock().unwrap();
            if let Some(pos) = f.iter().position(|x| x.slug == item.slug) {
                f.remove(pos);
                on = false;
            } else {
                f.insert(0, item);
                if f.len() > 5000 {
                    f.truncate(5000);
                }
                on = true;
            }
        }
        self.save_favorites();
        on
    }
}

/// 下载任务的完整流程：解析 -> 下载分片 -> 拼接 -> 转封装 -> 清理
pub async fn run_download(
    st: Arc<AppState>,
    task_id: String,
    url: String,
    out_dir: PathBuf,
    quality: String,
    threads: usize,
    cancel: Arc<AtomicBool>,
) {
    let result = do_download(&st, &task_id, &url, &out_dir, &quality,
                             threads, &cancel).await;
    finish(st, task_id, url, quality, result);
}

/// 把任务标记为完成/失败，并写历史。
fn finish(st: Arc<AppState>, task_id: String, url: String, quality: String,
          result: Result<PathBuf>) {
    match result {
        Ok(path) => {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let (slug, title, censor, cover) = {
                let g = st.tasks.lock().unwrap();
                match g.get(&task_id) {
                    Some(t) => (t.slug.clone(), t.title.clone(),
                                t.censor.clone(), t.cover.clone().unwrap_or_default()),
                    None => (String::new(), String::new(), None, String::new()),
                }
            };
            if let Some(t) = st.tasks.lock().unwrap().get_mut(&task_id) {
                t.status = "done".into();
                t.progress = 1.0;
                t.message = "完成".into();
                t.file = Some(path.to_string_lossy().to_string());
                t.bytes = size;
            }
            st.record_history(HistoryItem {
                slug: if slug.is_empty() { task_id.clone() } else { slug },
                title: if title.is_empty() {
                    path.file_stem().map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default()
                } else { title },
                url,
                quality,
                file: path.to_string_lossy().to_string(),
                size,
                censor,
                cover,
                status: "done".into(),
                finished: now(),
                missing: false,
            });
        }
        Err(e) => {
            let msg = e.to_string();
            let canceled = st.tasks.lock().unwrap().get(&task_id)
                .and_then(|t| t.cancel.clone())
                .map(|c| c.load(Ordering::Relaxed)).unwrap_or(false);
            if let Some(t) = st.tasks.lock().unwrap().get_mut(&task_id) {
                if canceled {
                    t.status = "canceled".into();
                    t.message = "已取消".into();
                } else {
                    t.status = "error".into();
                    t.error = Some(msg.clone());
                    t.message = msg.chars().take(200).collect();
                }
            }
        }
    }
}

/// 更新任务字段的辅助函数（加锁 + 忽略缺失）
fn patch(st: &Arc<AppState>, task_id: &str, f: impl FnOnce(&mut Task)) {
    if let Some(t) = st.tasks.lock().unwrap().get_mut(task_id) {
        f(t);
    }
}

async fn do_download(
    st: &Arc<AppState>,
    task_id: &str,
    url: &str,
    out_dir: &Path,
    quality: &str,
    threads: usize,
    cancel: &Arc<AtomicBool>,
) -> Result<PathBuf> {
    let proxy = st.proxy.lock().unwrap().clone();

    patch(st, task_id, |t| {
        t.status = "running".into();
        t.message = "解析播放源…".into();
    });

    // 1) 解析
    let page_f = Fetcher::new(proxy.clone(), 45, 3)?;
    let (m3u8, text) = {
        // 站点页需要代理；外部链接直连也行
        match site::resolve_stream(&page_f, url, None).await {
            Ok(v) => v,
            Err(e) => {
                if proxy.is_some() {
                    // 代理不通就退直连再试一次
                    let direct = Fetcher::new(None, 45, 2)?;
                    site::resolve_stream(&direct, url, None).await
                        .map_err(|_| e)?
                } else {
                    return Err(e);
                }
            }
        }
    };

    if cancel.load(Ordering::Relaxed) {
        return Err(anyhow!("已取消"));
    }

    // 2) 选清晰度、拿分片列表
    //
    // Referer 必须用**站点页地址**，不能用 m3u8 自身地址。
    // 实测：surrit CDN 对 m3u8 和分片都校验 Referer，
    // 不带站点页 URL 一律 403（无 Referer 403，带站点页 200）。
    let page_referer = url.to_string();
    let variants = hls::parse_master(&text, &m3u8);
    let (media_url, media_text) = if variants.is_empty() {
        (m3u8.clone(), text.clone())
    } else {
        let v = hls::pick_variant(&variants, quality)
            .ok_or_else(|| anyhow!("没有可用清晰度"))?;
        let t = page_f.get_text(&v.url, Some(&page_referer)).await?;
        (v.url.clone(), t)
    };
    let media = hls::parse_media(&media_text, &media_url);
    if media.segments.is_empty() {
        return Err(anyhow!("播放列表里没有分片"));
    }

    let seg_total = media.segments.len();
    patch(st, task_id, |t| {
        t.total_seg = seg_total;
        t.message = format!("{} 个分片", seg_total);
    });

    // 3) 输出名
    let stem = {
        let g = st.tasks.lock().unwrap();
        g.get(task_id).map(|t| {
            let s = if t.title.is_empty() { t.slug.clone() } else { t.title.clone() };
            sanitize_filename(&s)
        }).unwrap_or_else(|| task_id.to_string())
    };
    let stem = if stem.trim().is_empty() { task_id.to_string() } else { stem };
    std::fs::create_dir_all(out_dir).ok();

    // 4) 分片目录 —— 断点续传靠它，所以名字必须**跨次稳定**。
    //
    // 踩过的坑：原来用 `{m3u8}|{media_url}` 做哈希。但站点的播放源 URL
    // 里带一个一次性的 UUID（每次访问详情页都会变），于是同一个视频
    // 每次任务都会算出不同的目录名 —— 表现就是「下了 1998 片，
    // 重跑又从 0 开始」，断点续传和失败补片全部失效。
    //
    // 改用「番号 + 清晰度」作为稳定键：同一个视频、同一档清晰度，
    // 无论播放源怎么变，缓存目录都是同一个。
    let stable_key = {
        let g = st.tasks.lock().unwrap();
        let t = g.get(task_id);
        let slug = t.map(|x| x.slug.clone()).unwrap_or_default();
        let title = t.map(|x| x.title.clone()).unwrap_or_default();
        let q = t.map(|x| x.quality.clone()).unwrap_or_default();
        let ident = if !slug.is_empty() { slug } else if !title.is_empty() {
            title
        } else {
            url.to_string()
        };
        format!("{ident}|{q}")
    };
    let parts = out_dir.join(format!(".{stem}.parts-{}", short_hash(&stable_key)));
    std::fs::create_dir_all(&parts).ok();

    // 5) 下载
    let seg_fetcher = Fetcher::new(None, 45, 3)?;   // 分片直连，省代理流量
    let keystore = Arc::new(KeyStore::new(seg_fetcher.clone(), page_referer.clone()));

    let st_p = st.clone();
    let tid_p = task_id.to_string();
    let cb: Arc<dyn Fn(Progress) + Send + Sync> = Arc::new(move |p: Progress| {
        if let Some(t) = st_p.tasks.lock().unwrap().get_mut(&tid_p) {
            t.done_seg = p.done;
            t.total_seg = p.total;
            t.bytes = p.bytes;
            t.speed_bps = p.speed_bps;
            t.eta_secs = p.eta_secs;
            t.progress = if p.total > 0 { p.done as f64 / p.total as f64 } else { 0.0 };
            t.message = format!("分片 {}/{}", p.done, p.total);
        }
    });

    hls::download_segments(
        // 分片的 Referer 同样必须是站点页，否则 CDN 返 403
        seg_fetcher, &media.segments, keystore.clone(), &page_referer,
        &parts, threads, None, Some(cb),
    ).await?;

    if cancel.load(Ordering::Relaxed) {
        return Err(anyhow!("已取消"));
    }

    // 6) 拼接
    patch(st, task_id, |t| {
        t.status = "concat".into();
        t.message = "拼接分片…".into();
    });
    let ts_path = out_dir.join(format!("{stem}.ts"));
    let stc = st.clone();
    let tidc = task_id.to_string();
    let stage_cb: Arc<dyn Fn(&str, usize, usize) + Send + Sync> = Arc::new(move |ph, d, tot| {
        let mut g = stc.tasks.lock().unwrap();
        if let Some(t) = g.get_mut(&tidc) {
            t.status = ph.to_string();
            t.progress = if tot > 0 { d as f64 / tot as f64 } else { 0.0 };
            t.message = format!("拼接 {d}/{tot}");
        }
    });
    hls::concat_segments(&parts, &ts_path, seg_total, Some(stage_cb)).await?;

    if cancel.load(Ordering::Relaxed) {
        return Err(anyhow!("已取消"));
    }

    // 7) 转封装
    let mp4_path = out_dir.join(format!("{stem}.mp4"));
    if let Some(ff) = hls::find_ffmpeg() {
        patch(st, task_id, |t| {
            t.status = "remux".into();
            t.message = "转封装为 mp4…".into();
        });
        match hls::remux_with_ffmpeg(ff.to_string_lossy().as_ref(), &ts_path, &mp4_path).await {
            Ok(()) => {
                let _ = std::fs::remove_file(&ts_path);
            }
            Err(e) => {
                // 转封装失败不算致命：ts 也能播
                patch(st, task_id, |t| {
                    t.message = format!("转封装失败（保留 ts）: {e}");
                });
                let _ = std::fs::remove_dir_all(&parts);
                return Ok(ts_path);
            }
        }
    } else {
        // 没有 ffmpeg 就直接给 ts
        let _ = std::fs::rename(&ts_path, out_dir.join(format!("{stem}.ts")))
            .or_else(|_| Ok::<(), std::io::Error>(()));
        let _ = std::fs::remove_dir_all(&parts);
        return Ok(out_dir.join(format!("{stem}.ts")));
    }

    // 8) 清理缓存
    let _ = std::fs::remove_dir_all(&parts);
    Ok(mp4_path)
}

fn short_hash(s: &str) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..12].to_string()
}

pub fn sanitize_filename(s: &str) -> String {
    let bad = ['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\n', '\r', '\t'];
    let mut out: String = s.chars().map(|c| if bad.contains(&c) { '_' } else { c }).collect();
    out = out.trim().trim_matches('.').to_string();
    if out.chars().count() > 120 {
        out = out.chars().take(120).collect();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_illegal() {
        assert_eq!(sanitize_filename("a/b:c*d?"), "a_b_c_d_");
    }

    #[test]
    fn sanitize_trims_dots() {
        assert_eq!(sanitize_filename("..abc.."), "abc");
    }

    #[test]
    fn sanitize_limits_length() {
        let long = "x".repeat(300);
        assert!(sanitize_filename(&long).chars().count() <= 120);
    }

    #[test]
    fn short_hash_is_stable() {
        let a = short_hash("https://a.com/x");
        let b = short_hash("https://a.com/x");
        assert_eq!(a, b);
        assert_eq!(a.len(), 12);
        assert_ne!(a, short_hash("https://a.com/y"));
    }
}
