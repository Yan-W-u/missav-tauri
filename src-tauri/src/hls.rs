//! HLS（m3u8）解析、解密、分片下载、拼接、转封装。
//!
//! 对应原 Python 版 `missav_dl.py` 的下载核心。所有关键决策都从实测继承：
//!
//!   * 播放源候选分级：`source='...m3u8'` 最优先，广告位 `playlist.m3u8` 最次。
//!     踩过的坑：无码版和广告位抢排名，导致"选了无码下载下来还是有码"。
//!   * 分片伪装 `.jpeg` 扩展但内容是 TS（首字节 0x47）。
//!   * AES-128-CBC 解密，IV 来自 EXT-X-IV 或 MEDIA-SEQUENCE。
//!   * 滑窗并发：不能一次性提交全部分片 Future，否则内存爆到 GB 级。
//!   * 拼接用顺序追加写，不整块读入内存。

use crate::fetch::Fetcher;
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use anyhow::{anyhow, Result};
use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

// ─────────────────────────── 数据结构 ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub bandwidth: u64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub index: usize,
    pub url: String,
    /// byte-range (start, length)
    pub byterange: Option<(u64, u64)>,
    pub duration: f64,
    /// 本分片使用的密钥 URI（若加密）
    pub key_uri: Option<String>,
    /// 显式 IV（十六进制）
    pub iv: Option<[u8; 16]>,
}

#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    pub segments: Vec<Segment>,
    pub init_map: Option<String>,
    pub encrypted: bool,
    pub key_uris: Vec<String>,
    pub target_duration: f64,
    pub total_duration: f64,
}

// ─────────────────────────── 解析 ───────────────────────────

static RES_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"RESOLUTION=(\d+)x(\d+)").unwrap());
static BW_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"BANDWIDTH=(\d+)").unwrap());
static KEY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"#EXT-X-KEY:[^\n]*URI="([^"]+)""#).unwrap()
});
static IV_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"IV=0x([0-9a-fA-F]+)").unwrap());
static SEQ_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"#EXT-X-MEDIA-SEQUENCE:(\d+)").unwrap());
static BYTERANGE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"#EXT-X-BYTERANGE:(\d+)@(\d+)").unwrap());
static DUR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"#EXTINF:([\d.]+)").unwrap());

/// 解析 master playlist，按清晰度从高到低返回
pub fn parse_master(text: &str, base_url: &str) -> Vec<Variant> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.starts_with("#EXT-X-STREAM-INF") {
            let height = RES_RE.captures(line)
                .and_then(|c| c.get(2))
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let width = RES_RE.captures(line)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            let bandwidth = BW_RE.captures(line)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse::<u64>().ok())
                .unwrap_or(0);
            // 下一行非注释行就是地址
            let mut j = i + 1;
            while j < lines.len() && (lines[j].trim().is_empty()
                || lines[j].trim().starts_with('#'))
            {
                j += 1;
            }
            if j < lines.len() {
                if let Some(u) = resolve_url(base_url, lines[j].trim()) {
                    out.push(Variant {
                        url: u,
                        width,
                        height,
                        bandwidth,
                        name: if height > 0 {
                            format!("{height}p")
                        } else {
                            "原画".to_string()
                        },
                    });
                }
            }
            i = j;
        }
        i += 1;
    }
    out.sort_by(|a, b| b.height.cmp(&a.height).then(b.bandwidth.cmp(&a.bandwidth)));
    out
}

/// 解析 media playlist
pub fn parse_media(text: &str, base_url: &str) -> MediaInfo {
    let mut info = MediaInfo::default();
    let seq: u64 = SEQ_RE.captures(text)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(1);

    let mut cur_key: Option<String> = None;
    let mut cur_iv: Option<[u8; 16]> = None;
    let mut cur_dur: f64 = 0.0;
    let mut cur_range: Option<(u64, u64)> = None;
    let mut idx: u64 = 0;
    let mut keyset: Vec<String> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#EXT-X-TARGETDURATION:") {
            info.target_duration = line[22..].trim().parse().unwrap_or(0.0);
            continue;
        }
        if line.starts_with("#EXT-X-MAP:") {
            if let Some(u) = KEY_RE.captures(line).and_then(|c| c.get(1)) {
                info.init_map = resolve_url(base_url, u.as_str());
            }
            continue;
        }
        if line.starts_with("#EXT-X-KEY") {
            if let Some(c) = KEY_RE.captures(line) {
                let uri = resolve_url(base_url, &c[1]).unwrap_or_else(|| c[1].to_string());
                info.encrypted = true;
                cur_key = Some(uri.clone());
                if !keyset.contains(&uri) {
                    keyset.push(uri);
                }
            }
            cur_iv = IV_RE.captures(line)
                .and_then(|c| hex_to_iv(&c[1]));
            continue;
        }
        if line.starts_with("#EXTINF:") {
            cur_dur = DUR_RE.captures(line)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0.0);
            continue;
        }
        if line.starts_with("#EXT-X-BYTERANGE:") {
            cur_range = BYTERANGE_RE.captures(line).map(|c| {
                (c[2].parse().unwrap_or(0), c[1].parse().unwrap_or(0))
            });
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        // 分片地址行
        if let Some(u) = resolve_url(base_url, line) {
            // 未显式给 IV 时按规范用 media sequence 当 IV
            let iv = cur_iv.or_else(|| {
                if info.encrypted {
                    Some(seq_iv(seq + idx))
                } else {
                    None
                }
            });
            info.segments.push(Segment {
                index: idx as usize,
                url: u,
                byterange: cur_range.take(),
                duration: cur_dur,
                key_uri: cur_key.clone(),
                iv,
            });
            info.total_duration += cur_dur;
            idx += 1;
        }
    }
    info.key_uris = keyset;
    info
}

fn hex_to_iv(s: &str) -> Option<[u8; 16]> {
    let s = s.trim();
    if s.len() > 32 {
        return None;
    }
    let padded = format!("{:0>32}", s);
    let mut iv = [0u8; 16];
    for i in 0..16 {
        iv[i] = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(iv)
}

fn seq_iv(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

/// 相对地址转绝对
pub fn resolve_url(base: &str, rel: &str) -> Option<String> {
    if rel.is_empty() {
        return None;
    }
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return Some(rel.to_string());
    }
    if rel.starts_with("//") {
        // 协议相对：跟随 base 的协议，但不能误吃字符串里的 //
        let scheme = base.split("://").next().unwrap_or("https");
        return Some(format!("{scheme}:{rel}"));
    }
    let b = url::Url::parse(base).ok()?;
    b.join(rel).ok().map(|u| u.to_string())
}

/// 选清晰度：best / 1080p / 720p / 具体高度
pub fn pick_variant<'a>(variants: &'a [Variant], want: &str) -> Option<&'a Variant> {
    if variants.is_empty() {
        return None;
    }
    let w = want.trim().to_lowercase();
    if w == "best" || w.is_empty() {
        // 过滤纯音频（height == 0 且带宽很低）
        let with_video: Vec<&Variant> =
            variants.iter().filter(|v| v.height > 0).collect();
        return with_video.first().copied().or_else(|| variants.first());
    }
    let target: u32 = w.trim_end_matches('p').parse().unwrap_or(0);
    if target > 0 {
        // 优先精确匹配，否则选不高于目标的最高档
        if let Some(v) = variants.iter().find(|v| v.height == target) {
            return Some(v);
        }
        let mut cand: Vec<&Variant> = variants.iter().filter(|v| v.height <= target).collect();
        cand.sort_by(|a, b| b.height.cmp(&a.height));
        if let Some(v) = cand.first() {
            return Some(*v);
        }
    }
    variants.first()
}

// ─────────────────────────── 密钥缓存 ───────────────────────────

pub struct KeyStore {
    fetcher: Fetcher,
    referer: String,
    cache: Mutex<HashMap<String, Vec<u8>>>,
}

impl KeyStore {
    pub fn new(fetcher: Fetcher, referer: impl Into<String>) -> Self {
        Self { fetcher, referer: referer.into(), cache: Mutex::new(HashMap::new()) }
    }

    pub async fn get(&self, uri: &str) -> Result<Vec<u8>> {
        {
            let g = self.cache.lock().await;
            if let Some(k) = g.get(uri) {
                return Ok(k.clone());
            }
        }
        let raw = self.fetcher.get_bytes(uri, Some(&self.referer), 30).await?;
        // 有些站点返回 base64 文本而不是二进制
        let key = if raw.len() != 16 && !raw.is_empty() {
            base64::engine::general_purpose::STANDARD
                .decode(String::from_utf8_lossy(&raw).trim())
                .unwrap_or(raw)
        } else {
            raw
        };
        if key.len() != 16 {
            return Err(anyhow!("密钥长度异常（{} 字节，应为 16）", key.len()));
        }
        let mut g = self.cache.lock().await;
        g.insert(uri.to_string(), key.clone());
        Ok(key)
    }
}

// ─────────────────────────── 分片下载 ───────────────────────────

#[derive(Debug, Clone, Serialize, Default)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub bytes: u64,
    pub speed_bps: f64,
    pub eta_secs: f64,
}

/// 下载全部（或前 limit 个）分片到目录。
///
/// 滑窗提交：同时在途的分片数 = 线程数 × 2。
/// 为什么不是一次提交全部：2600 片 × 每片 460KB 的 Future 结果会让内存涨到
/// 5.6 GB，最后几片时 GC 压力叠加表现为"卡住不动"。
pub async fn download_segments(
    seg_fetcher: Fetcher,
    segments: &[Segment],
    keystore: Arc<KeyStore>,
    referer: &str,
    dst: &Path,
    threads: usize,
    limit: Option<usize>,
    progress_cb: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
) -> Result<usize> {
    tokio::fs::create_dir_all(dst).await.ok();
    let list: Vec<&Segment> = match limit {
        Some(n) => segments.iter().take(n).collect(),
        None => segments.iter().collect(),
    };
    let total = list.len();

    // 断点续传：已有非空分片跳过
    let mut todo: Vec<(usize, PathBuf)> = Vec::new();
    for seg in &list {
        let p = dst.join(format!("seg_{:05}.ts", seg.index));
        let skip = match tokio::fs::metadata(&p).await {
            Ok(m) => m.len() > 0,
            Err(_) => false,
        };
        if !skip {
            todo.push((seg.index, p));
        }
    }
    let already = total - todo.len();
    if todo.is_empty() {
        if let Some(cb) = &progress_cb {
            cb(Progress { done: total, total, ..Default::default() });
        }
        return Ok(total);
    }

    let done = Arc::new(AtomicU64::new(already as u64));
    let bytes = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();
    let window = (threads.max(1)) * 2;
    let seg_map: HashMap<usize, &Segment> =
        list.iter().map(|s| (s.index, *s)).collect();

    let sem = Arc::new(tokio::sync::Semaphore::new(window));
    let mut handles = Vec::new();

    for (idx, path) in todo {
        let permit = sem.clone().acquire_owned().await?;
        let seg = seg_map.get(&idx).copied().unwrap_or(&segments[0]).clone();
        let f = seg_fetcher.clone();
        let ks = keystore.clone();
        let ref_s = referer.to_string();
        let done_c = done.clone();
        let bytes_c = bytes.clone();
        let cb = progress_cb.clone();
        let total_c = total;

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let res = fetch_one_segment(&f, &seg, &ks, &ref_s).await;
            match res {
                Ok(data) => {
                    let mut fh = tokio::fs::File::create(&path).await?;
                    fh.write_all(&data).await?;
                    fh.flush().await?;
                    bytes_c.fetch_add(data.len() as u64, Ordering::Relaxed);
                    let d = done_c.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Some(cb) = cb.as_ref() {
                        let el = t0.elapsed().as_secs_f64().max(0.001);
                        let b = bytes_c.load(Ordering::Relaxed);
                        let speed = b as f64 / el;
                        let left = total_c as u64 - d;
                        cb(Progress {
                            done: d as usize,
                            total: total_c,
                            bytes: b,
                            speed_bps: speed,
                            eta_secs: if speed > 1.0 { left as f64 * 460_000.0 / speed }
                                       else { 0.0 },
                        });
                    }
                    Ok::<(), anyhow::Error>(())
                }
                Err(e) => Err(anyhow!("分片 {} 失败: {}", seg.index, e)),
            }
        }));
    }

    let mut first_err: Option<anyhow::Error> = None;
    for h in handles {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(anyhow!("任务异常: {e}"));
                }
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(total)
}

/// 取单个分片（含解密）
async fn fetch_one_segment(f: &Fetcher, seg: &Segment, ks: &KeyStore,
                           referer: &str) -> Result<Vec<u8>> {
    let mut data = f.get_bytes(&seg.url, Some(referer), 60).await?;
    if let (Some(uri), Some(iv)) = (seg.key_uri.as_ref(), seg.iv) {
        let key = ks.get(uri).await?;
        data = aes128_cbc_decrypt(&data, &key, &iv)?;
    }
    Ok(data)
}

fn aes128_cbc_decrypt(data: &[u8], key: &[u8], iv: &[u8; 16]) -> Result<Vec<u8>> {
    if key.len() != 16 {
        return Err(anyhow!("AES key 长度应为 16，实际 {}", key.len()));
    }
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut buf = data.to_vec();
    let pt = Aes128CbcDec::new(key.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow!("解密失败: {e}"))?;
    Ok(pt.to_vec())
}

/// 拼接分片为单个 ts 文件
pub async fn concat_segments(dst: &Path, out_file: &Path, count: usize,
                             stage_cb: Option<Arc<dyn Fn(&str, usize, usize) + Send + Sync>>)
    -> Result<u64>
{
    let mut out = tokio::fs::File::create(out_file).await?;
    let mut total: u64 = 0;
    for i in 0..count {
        let p = dst.join(format!("seg_{i:05}.ts"));
        let data = match tokio::fs::read(&p).await {
            Ok(d) => d,
            Err(_) => continue,
        };
        out.write_all(&data).await?;
        total += data.len() as u64;
        if let Some(cb) = &stage_cb {
            if i % 50 == 0 {
                cb("concat", i, count);
            }
        }
    }
    out.flush().await?;
    if let Some(cb) = &stage_cb {
        cb("concat", count, count);
    }
    Ok(total)
}

/// 用 ffmpeg 无损转封装（ts -> mp4）
pub async fn remux_with_ffmpeg(ffmpeg: &str, ts: &Path, mp4: &Path) -> Result<()> {
    let out = tokio::process::Command::new(ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error",
               "-i"]).arg(ts)
        .args(["-c", "copy", "-bsf:a", "aac_adtstoasc", "-movflags", "+faststart"])
        .arg(mp4)
        .output()
        .await
        .map_err(|e| anyhow!("启动 ffmpeg 失败: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("ffmpeg 转封装失败: {}", err.chars().take(300).collect::<String>()));
    }
    Ok(())
}

/// 找系统里的 ffmpeg
pub fn find_ffmpeg() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    for p in [
        r"D:\EVCapture\ffmpeg.exe",
        r"D:\CR-2000\Bin64\ffmpeg.exe",
        r"C:\ffmpeg\bin\ffmpeg.exe",
    ] {
        cands.push(PathBuf::from(p));
    }
    // PATH 里的第一个
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            let exe = dir.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
            if exe.is_file() {
                cands.push(exe);
            }
        }
    }
    cands.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
640x360/video.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1400000,RESOLUTION=842x480
842x480/video.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720
1280x720/video.m3u8
";

    #[test]
    fn master_sorted_desc() {
        let v = parse_master(MASTER, "https://cdn.example.com/p.mp4/playlist.m3u8");
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].height, 720);
        assert_eq!(v[2].height, 360);
    }

    #[test]
    fn pick_best_and_specific() {
        let v = parse_master(MASTER, "https://cdn.example.com/playlist.m3u8");
        assert_eq!(pick_variant(&v, "best").unwrap().height, 720);
        assert_eq!(pick_variant(&v, "360p").unwrap().height, 360);
        // 请求 1080 但只有 720 时给最高的
        assert_eq!(pick_variant(&v, "1080p").unwrap().height, 720);
    }

    #[test]
    fn media_parses_segments_and_relative_urls() {
        let txt = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\nseg0.jpeg\n#EXTINF:4.0,\nseg1.jpeg\n";
        let m = parse_media(txt, "https://surrit.com/uuid/1280x720/video.m3u8");
        assert_eq!(m.segments.len(), 2);
        assert!(m.segments[0].url.ends_with("seg0.jpeg"));
        assert!(m.segments[0].url.starts_with("https://surrit.com/"));
        assert_eq!(m.total_duration, 8.0);
    }

    #[test]
    fn media_detects_encryption_and_iv() {
        let txt = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x0123456789abcdef0123456789abcdef\n#EXTINF:4.0,\ns.ts\n";
        let m = parse_media(txt, "https://a.com/x/video.m3u8");
        assert!(m.encrypted);
        assert_eq!(m.key_uris.len(), 1);
        assert!(m.key_uris[0].ends_with("key.bin"));
        assert_eq!(m.segments[0].iv.unwrap()[0], 0x01);
    }

    #[test]
    fn protocol_relative_url() {
        let u = resolve_url("https://a.com/x.m3u8", "//cdn.b.com/p.ts").unwrap();
        assert_eq!(u, "https://cdn.b.com/p.ts");
    }

    #[test]
    fn seq_iv_roundtrip() {
        let iv = seq_iv(1);
        assert_eq!(iv[15], 1);
        assert_eq!(iv[0], 0);
    }

    #[test]
    fn audio_only_filtered_in_best() {
        let v = vec![
            Variant { url: "a".into(), width: 0, height: 0, bandwidth: 128000, name: "音频".into() },
            Variant { url: "b".into(), width: 1280, height: 720, bandwidth: 2000000, name: "720p".into() },
        ];
        assert_eq!(pick_variant(&v, "best").unwrap().height, 720);
    }
}
