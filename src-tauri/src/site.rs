//! 站点解析：列表页、详情页、播放源候选分级。
//!
//! 对应原 Python 版 `missav_core.py`。这里有整个项目最贵的一条经验：

//! **播放源必须分级**。站点页面里既有真播放源，也有广告位的 `playlist.m3u8`。
//! 如果只按出现顺序取第一个，无码版会被广告位抢占，结果是
//! "选了无码，下载下来还是有码"。分级顺序：
//!   tier0  `source='...m3u8'` 等显式赋值 —— 真播放源
//!   tier1  其它 packer 解包出的地址
//!   tier2  页面 HTML 里裸的 m3u8 链接
//!   tier3  从 uuid 推导
//!
//! 另外 site 用了 Dean Edwards packer 压缩播放源，需要解包。

use crate::fetch::{Fetcher, SITE, SITE_REFERER};
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─────────────────────────── 数据结构 ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Item {
    pub id: String,
    pub url: String,
    pub title: String,
    pub cover: String,
    pub duration: String,
    /// "uncensored" | "censored"
    pub censor: String,
    pub chinese_sub: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Detail {
    pub id: String,
    pub url: String,
    pub title: String,
    pub full_title: String,
    pub synopsis: String,
    pub cover: String,
    pub duration: String,
    pub release: String,
    pub censor: String,
    pub actresses: Vec<String>,
    pub genres: Vec<String>,
    pub playlist: String,
    pub stream_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub items: Vec<Item>,
    pub page: usize,
    pub pages: usize,
}

// ─────────────────────────── 正则 ───────────────────────────

static ITEM_RE: Lazy<Regex> = Lazy::new(|| {
    // 卡片：<a href="/cn/xxx" ...>...<img data-src="封面" ...>
    // 注意：raw string 用 r##"..."## —— 正则里有 "#（排除 # 和 ?），
    // 用 r#"..."# 会在 "# 处提前结束。
    Regex::new(r##"(?s)<a[^>]+href="(?P<href>/cn/[^"#?]+)"[^>]*>(?P<inner>.{0,1200}?)</a>"##).unwrap()
});
static IMG_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?:data-src|src)="([^"]+)""#).unwrap());
static DUR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(\d{1,2}:\d{2}:\d{2})").unwrap()
});
static TITLE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"<title[^>]*>(.*?)</title>"#).unwrap());
static H1_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?s)<h1[^>]*>(.*?)</h1>"#).unwrap());
static OG_DESC_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"<meta[^>]+property="og:description"[^>]+content="([^"]*)""#).unwrap()
});
static OG_COVER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"<meta[^>]+property="og:image"[^>]+content="([^"]*)""#).unwrap()
});
static ACTRESS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"href="/cn/actresses/[^"]*"[^>]*>([^<]{1,40})<"#).unwrap()
});
static GENRE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"href="/cn/genres/([^"/]+)""#).unwrap()
});
static REL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\d{4}-\d{2}-\d{2})").unwrap());
static PAGES_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[?&]page=(\d+)").unwrap());

// 播放源候选（分级用）
static TIER0_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?:source\d*|src|videoUrl|playUrl|file)\s*=\s*['"]([^'"]+\.m3u8[^'"]*)['"]"#).unwrap()
});
static ANY_M3U8_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"https?://[^\s'"<>\\]+\.m3u8[^\s'"<>\\]*"#).unwrap()
});
static UUID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})").unwrap()
});

// ─────────────────────────── 工具 ───────────────────────────

/// 规范化番号：SSIS-001 / ssis_001 / SSIS 001 -> ssis-001
pub fn normalize_id(s: &str) -> String {
    let t = s.trim().to_lowercase();
    // 已经是 URL 就取最后一段
    if t.contains("://") {
        if let Some(seg) = t.rsplit('/').find(|x| !x.is_empty()) {
            return seg.split('?').next().unwrap_or(seg).to_string();
        }
    }
    t.replace(['_', ' '], "-")
        .trim_matches('-')
        .to_string()
}

/// 判断有码 / 无码
pub fn classify_censor(id: &str, title: &str, has_uncensored_link: bool) -> String {
    let hay = format!("{} {}", id.to_lowercase(), title.to_lowercase());
    if hay.contains("uncensored") || hay.contains("leak") || has_uncensored_link {
        return "uncensored".into();
    }
    // FC2-PPV 是无码的主要来源
    if id.to_lowercase().starts_with("fc2") || hay.contains("fc2-ppv") {
        return "uncensored".into();
    }
    "censored".into()
}

/// 洗标题：去掉站点后缀，但**不能吃掉番号**
pub fn clean_title(t: &str) -> String {
    let mut s = t.to_string();
    for suf in [" - MissAV", "- MissAV", " | MissAV", "MissAV", " - Miss", "- Miss"] {
        if let Some(p) = s.rfind(suf) {
            s.truncate(p);
        }
    }
    s.trim().to_string()
}

fn unescape_html(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
}

fn strip_tags(s: &str) -> String {
    static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]*>").unwrap());
    unescape_html(TAG.replace_all(s, " ").trim())
}

// ─────────────────────────── 列表页 ───────────────────────────

/// 解析列表页（搜索页 / 标签页 / 分区页结构一致）
pub fn parse_items(html: &str, base: &str) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for cap in ITEM_RE.captures_iter(html) {
        let href = cap.name("href").map(|m| m.as_str()).unwrap_or("");
        let inner = cap.name("inner").map(|m| m.as_str()).unwrap_or("");
        // 排除导航类链接
        let slug = href.trim_start_matches("/cn/").trim_end_matches('/').to_string();
        if slug.is_empty()
            || slug.starts_with("search")
            || slug.starts_with("genres")
            || slug.starts_with("actresses")
            || slug.starts_with("pages")
            || slug.contains('/')
        {
            continue;
        }
        if !seen.insert(slug.clone()) {
            continue;
        }
        let cover = IMG_RE.captures(inner)
            .map(|c| c[1].to_string())
            .filter(|s| s.starts_with("http"))
            .unwrap_or_default();
        let duration = DUR_RE.captures(inner)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        let title = strip_tags(inner);
        // 标题里通常含番号；取不到就用 slug
        let id = if title.to_uppercase().contains(&slug.to_uppercase()) {
            slug.clone()
        } else {
            // 从标题开头提取 "XXX-123" 形式
            let up = title.to_uppercase();
            static ID_RE: Lazy<Regex> =
                Lazy::new(|| Regex::new(r"([A-Z0-9]{2,10}-\d{2,6})").unwrap());
            ID_RE.captures(&up)
                .map(|c| c[1].to_lowercase())
                .unwrap_or_else(|| slug.clone())
        };
        let chinese = inner.contains("中文字幕") || slug.contains("chinese-subtitle");
        let unc_link = html.contains(&format!("{slug}-uncensored-leak"));
        out.push(Item {
            id: id.clone(),
            url: format!("{base}{href}"),
            title: clean_title(&title),
            cover,
            duration,
            censor: classify_censor(&slug, &title, unc_link && slug.contains("uncensored")),
            chinese_sub: chinese,
            tag: None,
        });
    }
    out
}

/// 从页面里推断总页数
pub fn total_pages(html: &str) -> usize {
    PAGES_RE.captures_iter(html)
        .filter_map(|c| c[1].parse::<usize>().ok())
        .max()
        .unwrap_or(1)
        .max(1)
}

// ─────────────────────────── 播放源分级 ───────────────────────────

/// 解 Dean Edwards packer：`eval(function(p,a,c,k,e,d){...}('...',62,..,'..'.split('|')))`
///
/// 站点把 m3u8 地址压在这个壳里，不解包就找不到真播放源。
pub fn unpack_packer(html: &str) -> String {
    static PACK_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"eval\(function\(p,a,c,k,e,[dr]\)\{.*?\}\('([^']*)',(\d+),(\d+),'([^']*)'\.split\('\|'\)")
            .unwrap()
    });
    let mut result = html.to_string();
    if let Some(c) = PACK_RE.captures(html) {
        let p = &c[1];
        let a: u32 = c[2].parse().unwrap_or(62);
        let k: Vec<&str> = c[4].split('|').collect();
        let unpacked = base_n_decode(p, a, &k);
        result.push('\n');
        result.push_str(&unpacked);
    }
    result
}

fn base_n_decode(input: &str, radix: u32, words: &[&str]) -> String {
    static WORD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\w+\b").unwrap());
    WORD_RE.replace_all(input, |caps: &regex::Captures| {
        let w = &caps[0];
        if let Some(n) = parse_radix(w, radix) {
            if let Some(s) = words.get(n) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        w.to_string()
    }).to_string()
}

/// 按任意进制解析（2..=62）。
///
/// 为什么不用 u32::from_str_radix：Dean Edwards packer 用 **62 进制**
/// 编码（0-9 a-z A-Z），而标准库只支持到 36 进制。
/// 直接用会 panic："radix must lie in the range [2, 36] - found 62"。
fn parse_radix(s: &str, radix: u32) -> Option<usize> {
    if s.is_empty() || !(2..=62).contains(&radix) {
        return None;
    }
    let mut acc: u64 = 0;
    for ch in s.chars() {
        let d = match ch {
            '0'..='9' => ch as u32 - '0' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 10,
            'A'..='Z' => ch as u32 - 'A' as u32 + 36,
            _ => return None,
        };
        if d >= radix {
            return None;
        }
        acc = acc.checked_mul(radix as u64)?.checked_add(d as u64)?;
        if acc > usize::MAX as u64 {
            return None;
        }
    }
    Some(acc as usize)
}

/// 按分级提取 m3u8 候选
pub fn extract_m3u8_candidates(page_url: &str, html: &str) -> Vec<String> {
    let unpacked = unpack_packer(html);
    let mut tier0: Vec<String> = Vec::new();
    let mut tier1: Vec<String> = Vec::new();
    let mut tier2: Vec<String> = Vec::new();

    // tier0：显式赋值（真播放源）
    for c in TIER0_RE.captures_iter(&unpacked) {
        if let Some(u) = crate::hls::resolve_url(page_url, &c[1]) {
            if !tier0.contains(&u) {
                tier0.push(u);
            }
        }
    }
    // tier1：解包后出现的其它 m3u8
    for m in ANY_M3U8_RE.find_iter(&unpacked) {
        let u = m.as_str().to_string();
        if !tier0.contains(&u) && !tier1.contains(&u) {
            tier1.push(u);
        }
    }
    // tier2：原页面里的裸链接
    for m in ANY_M3U8_RE.find_iter(html) {
        let u = m.as_str().to_string();
        if !tier0.contains(&u) && !tier1.contains(&u) && !tier2.contains(&u) {
            tier2.push(u);
        }
    }

    // 广告位的 playlist.m3u8 排到最后（这是"无码变有码"的根因）
    let is_ad = |u: &str| u.contains("/playlist.m3u8") && !u.contains("surrit.com") ;
    tier0.sort_by_key(|u| is_ad(u));
    tier1.sort_by_key(|u| is_ad(u));

    let mut out = tier0;
    out.extend(tier1);
    out.extend(tier2);
    // tier3：从页面里出现的 uuid 推导 surrit 地址
    for c in UUID_RE.captures_iter(&unpacked) {
        let u = format!("https://surrit.com/{}/playlist.m3u8", &c[1]);
        if !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

// ─────────────────────────── 详情页 ───────────────────────────

pub async fn fetch_detail(f: &Fetcher, id_or_url: &str, referer: Option<&str>) -> Result<Detail> {
    let url = if id_or_url.starts_with("http") {
        id_or_url.to_string()
    } else {
        format!("{SITE}/cn/{}", normalize_id(id_or_url))
    };
    let ref_s = referer.unwrap_or(SITE_REFERER);
    let html = f.get_text(&url, Some(ref_s)).await?;

    let full_title = H1_RE.captures(&html)
        .map(|c| strip_tags(&c[1]))
        .unwrap_or_else(|| {
            TITLE_RE.captures(&html)
                .map(|c| clean_title(&unescape_html(&c[1])))
                .unwrap_or_default()
        });

    let synopsis = OG_DESC_RE.captures(&html)
        .map(|c| unescape_html(&c[1]))
        .or_else(|| {
            static DESC_RE: Lazy<Regex> = Lazy::new(|| {
                Regex::new(r#"<meta[^>]+name="description"[^>]+content="([^"]*)""#).unwrap()
            });
            DESC_RE.captures(&html).map(|c| unescape_html(&c[1]))
        })
        .unwrap_or_default();

    let cover = OG_COVER_RE.captures(&html)
        .map(|c| c[1].to_string())
        .unwrap_or_default();

    let mut actresses: Vec<String> = Vec::new();
    for c in ACTRESS_RE.captures_iter(&html) {
        let n = strip_tags(&c[1]);
        if !n.is_empty() && !actresses.contains(&n) {
            actresses.push(n);
        }
    }
    let mut genres: Vec<String> = Vec::new();
    for c in GENRE_RE.captures_iter(&html) {
        let g = c[1].to_string();
        let g = percent_encoding::percent_decode_str(&g).decode_utf8_lossy().to_string();
        if !g.is_empty() && !genres.contains(&g) {
            genres.push(g);
        }
    }

    let release = REL_RE.captures(&html)
        .map(|c| c[1].to_string()).unwrap_or_default();
    let duration = DUR_RE.captures(&html)
        .map(|c| c[1].to_string()).unwrap_or_default();

    let cands = extract_m3u8_candidates(&url, &html);
    let playlist = cands.first().cloned().unwrap_or_default();
    let stream_id = playlist_uuid(&playlist);
    let slug = normalize_id(&url);
    let has_unc = html.contains("uncensored-leak");
    // 先算 censor 再移动 full_title（否则借用已移走的值）
    let censor = classify_censor(&slug, &full_title,
                                 has_unc && slug.contains("uncensored"));

    Ok(Detail {
        id: slug,
        url,
        title: clean_title(&full_title),
        full_title,
        synopsis,
        cover,
        duration,
        release,
        censor,
        actresses,
        genres,
        playlist,
        stream_id,
    })
}

pub fn playlist_uuid(playlist: &str) -> String {
    UUID_RE.captures(playlist)
        .map(|c| c[1].to_string())
        .unwrap_or_default()
}

/// 一轮"解析出可用 m3u8"：候选逐个探测，取第一个真的能拿到的。
pub async fn resolve_stream(f: &Fetcher, page_url: &str, referer: Option<&str>)
    -> Result<(String, String)>
{
    let ref_s = referer.unwrap_or(page_url);
    // 本身就是 m3u8
    if page_url.split('?').next().unwrap_or("").to_lowercase().ends_with(".m3u8") {
        let t = f.get_text(page_url, Some(ref_s)).await?;
        if t.trim_start().to_uppercase().starts_with("#EXTM3U") {
            return Ok((page_url.to_string(), t));
        }
        return Err(anyhow!("该地址返回的不是 m3u8"));
    }
    let html = f.get_text(page_url, Some(ref_s)).await?;
    let cands = extract_m3u8_candidates(page_url, &html);
    for c in cands.iter().take(12) {
        // 探测：能拿到 #EXTM3U 才算数
        match f.get_text(c, Some(page_url)).await {
            Ok(t) if t.trim_start().to_uppercase().starts_with("#EXTM3U") => {
                return Ok((c.clone(), t));
            }
            _ => continue,
        }
    }
    Err(anyhow!(
        "未能在页面里找到 m3u8。常见原因：\n\
         1) 页面被 Cloudflare 人机验证挡住\n\
         2) 播放器用 JS 动态加载，地址不在 HTML 里\n\
         3) 需要登录才能看"))
}

// ─────────────────────────── 站点客户端 ───────────────────────────

pub struct SiteClient {
    pub f: Fetcher,
    pub base: String,
}

impl SiteClient {
    pub fn new(proxy: Option<String>, timeout: u64, retries: u32) -> Result<Self> {
        Ok(Self {
            f: Fetcher::new(proxy, timeout, retries)?,
            base: SITE.to_string(),
        })
    }

    pub async fn search_page(&self, kw: &str, page: usize) -> Result<Page> {
        let k = normalize_id(kw);
        if k.is_empty() {
            return Ok(Page { items: vec![], page, pages: 1 });
        }
        let base_url = format!("{}/cn/search/{}", self.base,
                               percent_encoding::utf8_percent_encode(
                                   &k, percent_encoding::NON_ALPHANUMERIC));
        let url = if page <= 1 { base_url } else { format!("{base_url}?page={page}") };
        let html = self.f.get_text(&url, Some(SITE_REFERER)).await?;
        let items = parse_items(&html, &self.base);
        Ok(Page { items, page, pages: total_pages(&html) })
    }

    pub async fn genre_page(&self, tag: &str, page: usize) -> Result<Page> {
        let enc = percent_encoding::utf8_percent_encode(tag,
            percent_encoding::NON_ALPHANUMERIC).to_string();
        let base_url = format!("{}/cn/genres/{}", self.base, enc);
        let url = if page <= 1 { base_url } else { format!("{base_url}?page={page}") };
        let html = self.f.get_text(&url, Some(SITE_REFERER)).await?;
        let items = parse_items(&html, &self.base);
        Ok(Page { items, page, pages: total_pages(&html) })
    }

    /// 多标签：站点只支持单标签查询，本地聚合。
    /// union = 任一标签；intersect = 同时具备（取交集）。
    pub async fn genre_multi(&self, tags: &[String], mode: &str, page: usize)
        -> Result<Page>
    {
        let mut all: Vec<Item> = Vec::new();
        let mut pages = 1usize;
        for t in tags {
            if let Ok(p) = self.genre_page(t, page).await {
                pages = pages.max(p.pages);
                for mut it in p.items {
                    it.tag = Some(t.clone());
                    all.push(it);
                }
            }
        }
        let mut out: Vec<Item> = Vec::new();
        if mode == "intersect" {
            let mut cnt: HashMap<String, usize> = HashMap::new();
            for it in &all {
                *cnt.entry(it.id.clone()).or_insert(0) += 1;
            }
            let mut seen = std::collections::HashSet::new();
            for it in all {
                if cnt.get(&it.id).copied().unwrap_or(0) >= tags.len()
                    && seen.insert(it.id.clone())
                {
                    out.push(it);
                }
            }
        } else {
            let mut seen = std::collections::HashSet::new();
            for it in all {
                if seen.insert(it.id.clone()) {
                    out.push(it);
                }
            }
        }
        Ok(Page { items: out, page, pages })
    }

    pub async fn section_page(&self, sec: &str, page: usize) -> Result<Page> {
        let path = match sec {
            "new" => "/cn/new",
            "release" => "/cn/release",
            "uncensored-leak" => "/cn/uncensored-leak",
            "chinese-subtitle" => "/cn/chinese-subtitle",
            _ => return Err(anyhow!("未知分区 {sec}")),
        };
        let url = if page <= 1 {
            format!("{}{}", self.base, path)
        } else {
            format!("{}{}?page={}", self.base, path, page)
        };
        let html = self.f.get_text(&url, Some(SITE_REFERER)).await?;
        let items = parse_items(&html, &self.base);
        Ok(Page { items, page, pages: total_pages(&html) })
    }

    /// 标签总表
    pub async fn genres(&self) -> Result<Vec<Genre>> {
        let html = self.f.get_text(&format!("{}/cn/genres", self.base),
                                   Some(SITE_REFERER)).await?;
        static PAIR_RE: Lazy<Regex> = Lazy::new(|| {
            Regex::new(r#"/genres/([^"'<>]+)"[^>]*>\s*([^<]{1,40}?)\s*<(?:(?!</?a>)[\s\S]{0,400}?)(\d+)\s*条影片"#).unwrap()
        });
        let mut out = Vec::new();
        for c in PAIR_RE.captures_iter(&html) {
            let slug = percent_encoding::percent_decode_str(&c[1])
                .decode_utf8_lossy().to_string();
            let name = strip_tags(&c[2]);
            let count: u64 = c[3].parse().unwrap_or(0);
            if !name.is_empty() {
                out.push(Genre { tag: name, slug, count });
            }
        }
        Ok(out)
    }

    pub async fn detail(&self, id_or_url: &str) -> Result<Detail> {
        fetch_detail(&self.f, id_or_url, None).await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Genre {
    pub tag: String,
    pub slug: String,
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_variants() {
        assert_eq!(normalize_id("SSIS-001"), "ssis-001");
        assert_eq!(normalize_id("ssis_001"), "ssis-001");
        assert_eq!(normalize_id("SSIS 001"), "ssis-001");
        assert_eq!(normalize_id("https://missav.live/cn/ssis-001"),
                   "ssis-001");
    }

    #[test]
    fn censor_classification() {
        assert_eq!(classify_censor("ssis-001", "普通", false), "censored");
        assert_eq!(classify_censor("ssis-001-uncensored-leak", "", false), "uncensored");
        assert_eq!(classify_censor("fc2-ppv-123", "", false), "uncensored");
    }

    #[test]
    fn clean_title_keeps_id() {
        // 关键回归：洗标题不能把番号吃掉
        let t = "SSIS-001 女友不在的三天 - MissAV";
        let c = clean_title(t);
        assert!(c.contains("SSIS-001"), "番号被吃掉了: {c}");
        assert!(!c.contains("MissAV"));
    }

    #[test]
    fn parse_radix_base62() {
        // 标准库只支持到 36，base62 必须自己算
        assert_eq!(parse_radix("0", 62), Some(0));
        assert_eq!(parse_radix("9", 62), Some(9));
        assert_eq!(parse_radix("a", 62), Some(10));
        assert_eq!(parse_radix("z", 62), Some(35));
        assert_eq!(parse_radix("A", 62), Some(36));
        assert_eq!(parse_radix("Z", 62), Some(61));
        assert_eq!(parse_radix("10", 62), Some(62));
        // 非法字符 / 超出进制
        assert_eq!(parse_radix("!", 62), None);
        // 36 进制下 z 是合法值(35);但大写 A 在 36 进制里也是 36 >= 36 -> 非法
        assert_eq!(parse_radix("z", 36), Some(35));
        assert_eq!(parse_radix("A", 36), None);
    }

    #[test]
    fn unpack_dean_edwards() {
        // 真实形态的 packer 壳（简化版）：变量表里有 m3u8
        let packed = "eval(function(p,a,c,k,e,d){e=function(c){return c};return p}('0://1.2/3.m3u8',62,4,'https|surrit|com|playlist'.split('|')"
            ;
        let out = unpack_packer(packed);
        assert!(out.contains("https://surrit.com/playlist.m3u8"),
                "解包失败: {out}");
    }

    #[test]
    fn candidates_tier_ad_last() {
        // 广告位 playlist.m3u8 必须排在后面
        let html = r#"
        <script>var x = 'https://ad.example.com/playlist.m3u8';</script>
        <script>source = 'https://surrit.com/UUID/1280x720/video.m3u8';</script>
        "#;
        let c = extract_m3u8_candidates("https://missav.live/cn/x", html);
        assert!(!c.is_empty());
        assert!(c[0].contains("surrit.com"), "tier0 应排第一: {c:?}");
    }

    #[test]
    fn parse_items_from_fixture() {
        let html = r#"
        <a href="/cn/ssis-001" class="group">
          <img data-src="https://fourhoi.com/ssis-001/cover-t.jpg">
          <span>1:58:03</span>
          <div>SSIS-001 测试标题</div>
        </a>
        "#;
        let items = parse_items(html, "https://missav.live");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "ssis-001");
        assert!(items[0].cover.contains("fourhoi"));
        assert_eq!(items[0].duration, "1:58:03");
    }

    #[test]
    fn total_pages_from_links() {
        let html = r#"<a href="?page=2">2</a><a href="?page=160">160</a>"#;
        assert_eq!(total_pages(html), 160);
    }
}
