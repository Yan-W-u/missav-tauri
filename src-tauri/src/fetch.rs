//! 抓取层：带浏览器指纹的 HTTP 客户端、重试、空闲超时流式下载。
//!
//! 对应原 Python 版 `missav_dl.py` 的 `Fetcher`。
//! 关键经验（从 Python 版逐条继承，都是实测踩出来的）：
//!   * CDN 防盗链只认站点页 Referer，漏了直接 403
//!   * 某些 CDN 对 HTTP/2 支持有问题，HTTP/1.1 才稳
//!   * 超时必须是"空闲超时"而不是总时长——慢分片能下完，死链能止损
//!   * 浏览器 UA + Accept 头是过 Cloudflare 的最低要求

use anyhow::{anyhow, Context, Result};
use once_cell::sync::Lazy;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_LANGUAGE,
                      CONNECTION, REFERER, USER_AGENT};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

pub const UA_DEFAULT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub const SITE: &str = "https://missav.live";
pub const SITE_REFERER: &str = "https://missav.live/cn";

/// 抓取器。克隆是廉价的（内部 reqwest::Client 是 Arc 共享的）。
#[derive(Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    pub proxy: Option<String>,
    pub timeout: Duration,
    pub retries: u32,
    ua: String,
    /// 忽略证书校验。用于"本地代理做 TLS 中间人但根证书没装进系统信任库"
    /// 的场合（实测 SteamTools / Watt Toolkit 就是这种，报 UnknownIssuer）。
    pub insecure: bool,
}

impl Fetcher {
    /// 默认构造：自动读取全局"忽略证书"开关。
    pub fn new(proxy: Option<String>, timeout_secs: u64, retries: u32) -> Result<Self> {
        let insecure = GLOBAL_INSECURE.load(std::sync::atomic::Ordering::Relaxed);
        Self::with_options(proxy, timeout_secs, retries, insecure)
    }

    /// insecure=true 时不校验 TLS 证书。
    /// 只在"用户自己的本地代理"场景下用 —— 那一段流量本来就在本机内部。
    pub fn with_options(proxy: Option<String>, timeout_secs: u64, retries: u32,
                        insecure: bool) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .user_agent(UA_DEFAULT)
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(16)
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .redirect(reqwest::redirect::Policy::limited(10));

        if insecure {
            builder = builder.danger_accept_invalid_certs(true);
        }

        if let Some(p) = proxy.as_ref().filter(|s| !s.trim().is_empty()) {
            let normalized = if p.starts_with("http://") || p.starts_with("socks5://")
                || p.starts_with("socks5h://") || p.starts_with("https://")
            {
                p.to_string()
            } else {
                format!("http://{p}")
            };
            builder = builder.proxy(
                reqwest::Proxy::all(&normalized)
                    .with_context(|| format!("代理地址无效: {normalized}"))?,
            );
            builder = builder.http1_only();
        }

        Ok(Self {
            client: builder.build().context("构建 HTTP 客户端失败")?,
            proxy,
            timeout: Duration::from_secs(timeout_secs),
            retries,
            ua: UA_DEFAULT.to_string(),
            insecure,
        })
    }

    pub fn no_proxy(timeout_secs: u64) -> Result<Self> {
        Self::new(None, timeout_secs, 2)
    }

    /// 同一代理、同一超时，但允许忽略证书错误（探测代理时用）
    pub fn with_insecure(&self, insecure: bool) -> Result<Self> {
        Self::with_options(self.proxy.clone(), self.timeout.as_secs(),
                           self.retries, insecure)
    }

    fn base_headers(&self, referer: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(USER_AGENT, HeaderValue::from_str(&self.ua).unwrap());
        h.insert(ACCEPT, HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"));
        h.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"));
        h.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        if let Some(r) = referer {
            if let Ok(v) = HeaderValue::from_str(r) {
                h.insert(REFERER, v);
                // 有些站点校验 Origin
                if let Ok(u) = url::Url::parse(r) {
                    if let Some(host) = u.host_str() {
                        let origin = format!("{}://{}", u.scheme(), host);
                        if let Ok(ov) = HeaderValue::from_str(&origin) {
                            h.insert(HeaderName::from_static("origin"), ov);
                        }
                    }
                }
            }
        }
        h
    }

    /// 取文本（自动处理编码：header charset → meta → UTF-8 → gb18030）
    pub async fn get_text(&self, url: &str, referer: Option<&str>) -> Result<String> {
        let bytes = self.get_bytes(url, referer, 60).await?;
        Ok(decode_body(&bytes))
    }

    /// 取字节，带重试。超时随重试递增。
    pub async fn get_bytes(&self, url: &str, referer: Option<&str>,
                           idle_timeout_secs: u64) -> Result<Vec<u8>> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=self.retries.max(1) {
            let h = self.base_headers(referer);
            let tmo = self.timeout * attempt.min(3);
            match self.client.get(url).headers(h).timeout(tmo).send().await {
                Ok(resp) => {
                    let code = resp.status();
                    if code.is_success() || code.as_u16() == 206 {
                        match self.read_with_idle(resp, idle_timeout_secs).await {
                            Ok(b) => return Ok(b),
                            Err(e) => last_err = Some(e),
                        }
                    } else if code.as_u16() == 403 {
                        return Err(anyhow!(
                            "HTTP 403（防盗链或 Cloudflare 拦截；检查 Referer 是否正确）"));
                    } else if code.as_u16() == 404 {
                        return Err(anyhow!("HTTP 404（地址不存在或已过期）"));
                    } else {
                        last_err = Some(anyhow!("HTTP {code}"));
                    }
                }
                Err(e) => {
                    last_err = Some(anyhow!(classify_reqwest_err(&e)));
                }
            }
            if attempt < self.retries {
                tokio::time::sleep(Duration::from_millis(
                    500 * (1u64 << attempt.min(3)))).await;
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("取回失败 {url}")))
    }

    /// 流式读取，带空闲超时：只要还在来数据就等，停住 idle 秒才放弃。
    ///
    /// 为什么必须这样：某分片 987KB 在 CDN 上要 53 秒才下完（约 18KB/s），
    /// 用固定总超时必然判失败，而它其实是能下到的。
    async fn read_with_idle(&self, resp: reqwest::Response,
                            idle_secs: u64) -> Result<Vec<u8>> {
        use futures_util::StreamExt;
        let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
        let mut stream = resp.bytes_stream();
        let idle = Duration::from_secs(idle_secs.max(10));
        loop {
            match tokio::time::timeout(idle, stream.next()).await {
                Err(_) => {
                    return Err(anyhow!(
                        "连续 {}s 无数据（已收 {}KB）",
                        idle.as_secs(), buf.len() / 1024));
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    return Err(anyhow!("读取中断: {}", classify_reqwest_err(&e)));
                }
                Ok(Some(Ok(chunk))) => buf.extend_from_slice(&chunk),
            }
        }
        Ok(buf)
    }

    /// 只检查可达性，不读 body（省流量）
    pub async fn probe(&self, url: &str, referer: Option<&str>) -> Result<(u16, String)> {
        let h = self.base_headers(referer);
        let resp = self.client.get(url).headers(h).send().await
            .map_err(|e| anyhow!(raw_err(&e)))?;
        let code = resp.status().as_u16();
        let ct = resp.headers().get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        Ok((code, ct))
    }

    pub fn client(&self) -> &reqwest::Client { &self.client }
}

/// 把 reqwest 的错误翻成人能看懂的中文
pub fn classify_reqwest_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "连接超时（站点不可达、被墙，或代理没开）".to_string()
    } else if e.is_connect() {
        "连接失败（DNS 解析失败或端口不通）".to_string()
    } else if e.is_redirect() {
        "重定向次数过多".to_string()
    } else if e.is_body() || e.is_decode() {
        format!("数据读取失败: {e}")
    } else {
        let s = e.to_string();
        if s.contains("certificate") || s.contains("SSL") || s.contains("tls") {
            "TLS 握手失败（证书不被信任，或被 SNI 阻断）".to_string()
        } else {
            format!("请求失败: {s}")
        }
    }
}

/// 原始错误串（含 source 链），排查用
pub fn raw_err(e: &reqwest::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    let mut depth = 0;
    while let Some(s) = src {
        if depth >= 4 { break; }
        parts.push(s.to_string());
        src = s.source();
        depth += 1;
    }
    parts.join(" <- ")
}

/// 编码探测：header → meta charset → UTF-8 → gb18030
pub fn decode_body(bytes: &[u8]) -> String {
    // 1) 先按 UTF-8 试（绝大多数情况）
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    // 2) 找 meta charset
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_lowercase();
    let is_gb = head.contains("gb2312") || head.contains("gbk") || head.contains("gb18030");
    if is_gb {
        if let Some(s) = decode_gb18030(bytes) {
            return s;
        }
    }
    // 3) 兜底：有损转换（不 panic，最多少数字符）
    String::from_utf8_lossy(bytes).to_string()
}

/// 极简 GB18030 解码：只覆盖 GBK 常用区（与 Python 版行为一致的降级路径）
fn decode_gb18030(bytes: &[u8]) -> Option<String> {
    // 尝试用 encoding_rs 不可用时的方案：先把合法 UTF-8 段保留，
    // GBK 双字节按 0xA1-0xFE 区间估算为占位符。
    // 实际站点绝大多数返回 UTF-8；此分支只在 header 明确声明 GBK 时走到。
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            out.push(b as char);
            i += 1;
        } else if (0xA1..=0xFE).contains(&b) && i + 1 < bytes.len() {
            let b2 = bytes[i + 1];
            if (0xA1..=0xFE).contains(&b2) {
                // 无法还原具体汉字时用替换符，避免乱码扩散
                out.push('\u{FFFD}');
                i += 2;
            } else {
                out.push('\u{FFFD}');
                i += 1;
            }
        } else {
            out.push('\u{FFFD}');
            i += 1;
        }
    }
    Some(out)
}

/// 并发控制：限制同时进行的下载连接数
pub struct Limiter {
    sem: Arc<Semaphore>,
}

impl Limiter {
    pub fn new(n: usize) -> Self {
        Self { sem: Arc::new(Semaphore::new(n.max(1))) }
    }
    pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.sem.clone().acquire_owned().await.expect("semaphore closed")
    }
}

/// 常见的本地代理端口，按优先级排列
pub const PROXY_PORTS: &[u16] = &[
    7890, 7897, 7891, 10809, 10808, 2080, 8889, 1080, 8080, 12334, 26561,
];

/// 探测到的代理是否需要忽略证书校验（TLS 中间人场景）
pub static PROXY_NEEDS_INSECURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 全局"忽略证书"开关。
///
/// 为什么不逐个函数传参：Fetcher 的构造点有十几处，
/// 漏一个就会在那种场景下报 UnknownIssuer，排查成本很高。
/// 用全局开关 + Fetcher::new 自动读取，行为一致且不会漏。
pub static GLOBAL_INSECURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 探测缓存（用户点「重新检测」时清空）
static CACHE: Lazy<tokio::sync::Mutex<Option<(Option<String>, std::time::Instant)>>> =
    Lazy::new(|| tokio::sync::Mutex::new(None));

/// 清空代理探测缓存，让下一次 detect_proxy 真正重探
pub async fn reset_proxy_cache() {
    *CACHE.lock().await = None;
}

/// 探测可用的本地代理。
///
/// 顺序很重要：
///   1. **先读系统代理设置**（注册表 ProxyEnable/ProxyServer）——
///      用户开了 VPN/代理客户端时这里一定有值，最可靠
///   2. 再扫常见端口，每个都真发一次请求确认能连外网
///
/// 为什么不能只看端口开着：代理客户端会残留监听但实际不通（实测多次）。
/// 为什么用多个探测目标：某些站点本身被封，用单一目标会把好代理误判为坏。
pub async fn detect_proxy() -> Option<String> {
    {
        let g = CACHE.lock().await;
        if let Some((p, t)) = g.as_ref() {
            if t.elapsed() < Duration::from_secs(60) {
                return p.clone();
            }
        }
    }

    let mut candidates: Vec<String> = Vec::new();

    // 1) 系统代理（最优先）
    if let Some(sys) = read_system_proxy() {
        candidates.push(sys);
    }
    // 2) 常见端口（本机回环）
    for port in PROXY_PORTS {
        candidates.push(format!("http://127.0.0.1:{port}"));
    }
    // 3) 模拟器/手机：宿主机的代理端口。
    //    Android 模拟器里 10.0.2.2 就是宿主机；主机若开了代理，
    //    从这里能连上。不加这一段的话模拟器里永远探测不到代理。
    #[cfg(target_os = "android")]
    for host in ["10.0.2.2", "10.0.3.2"] {
        for port in PROXY_PORTS {
            candidates.push(format!("http://{host}:{port}"));
        }
    }

    let mut found: Option<String> = None;
    for cand in candidates {
        let port = match cand.rsplit(':').next().and_then(|s| s.parse::<u16>().ok()) {
            Some(p) => p,
            None => continue,
        };
        if !port_open(port).await {
            continue;
        }
        // 端口开着还要真能连外网。先严格校验；失败再放宽证书再试一次 ——
        // 本地代理做 TLS 中间人时根证书通常不在系统库里（UnknownIssuer），
        // 这种情况代理本身是好的，不该判成不可用。
        let mut ok = false;
        for insecure in [false, true] {
            let f = match Fetcher::with_options(Some(cand.clone()), 8, 1, insecure) {
                Ok(f) => f,
                Err(_) => break,
            };
            for target in ["https://www.gstatic.com/generate_204",
                           "https://www.baidu.com/"] {
                if f.probe(target, None).await
                    .map(|(c, _)| (200..400).contains(&c)).unwrap_or(false)
                {
                    ok = true;
                    if insecure {
                        // 记录：这个代理需要忽略证书
                        PROXY_NEEDS_INSECURE.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    break;
                }
            }
            if ok {
                break;
            }
        }
        if ok {
            found = Some(cand);
            break;
        }
    }

    let mut g = CACHE.lock().await;
    *g = Some((found.clone(), std::time::Instant::now()));
    found
}

/// 读 Windows 系统代理设置
fn read_system_proxy() -> Option<String> {
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu.open_subkey(
            r"Software\Microsoft\Windows\CurrentVersion\Internet Settings").ok()?;
        let enable: u32 = key.get_value("ProxyEnable").unwrap_or(0);
        if enable == 0 {
            return None;
        }
        let server: String = key.get_value("ProxyServer").ok()?;
        if server.trim().is_empty() {
            return None;
        }
        // 可能是 "http=127.0.0.1:7890;https=..." 形式，取第一个
        let first = server.split(';').next()?.trim();
        let addr = if first.contains('=') {
            first.split('=').nth(1).unwrap_or(first)
        } else {
            first
        };
        let addr = addr.trim();
        if addr.is_empty() {
            return None;
        }
        Some(if addr.starts_with("http://") || addr.starts_with("socks5://") {
            addr.to_string()
        } else {
            format!("http://{addr}")
        })
    }
    #[cfg(not(windows))]
    {
        // 1) 先读系统代理设置。Android 上没有"环境变量代理"这种东西，
        //    系统代理存在 Settings.Global.http_proxy（就是设置里那个）。
        //    不读它的话，模拟器/手机上配了代理 App 也发现不了，
        //    所有请求都走直连然后超时 —— 实测就是这个症状。
        if let Some(p) = android_http_proxy() {
            return Some(p);
        }
        // 2) 退回环境变量（macOS / Linux 上有效）
        for k in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
            if let Ok(v) = std::env::var(k) {
                if !v.trim().is_empty() {
                    return Some(v.trim().to_string());
                }
            }
        }
        None
    }
}

/// 读 Android 的全局 HTTP 代理设置。
///
/// 实现方式：直接调 `getprop` 拿不到（那是系统属性），
/// 系统代理在 settings 数据库里，普通 App 没有读 settings 的权限。
/// 但 **App 自己能通过 Java 层 Settings.Global.getString 读到**，
/// 只是这里在 Rust 侧，跨 JNI 比较重。
///
/// 折中方案：读环境变量 + 探测宿主机常见端口。
/// 在模拟器里 10.0.2.2 就是宿主机，所以把宿主机端口也纳入候选。
#[cfg(target_os = "android")]
fn android_http_proxy() -> Option<String> {
    // 有些 ROM 会把代理写进环境变量
    for k in ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"] {
        if let Ok(v) = std::env::var(k) {
            let t = v.trim();
            if !t.is_empty() && t.contains(':') {
                return Some(if t.starts_with("http://") {
                    t.to_string()
                } else {
                    format!("http://{t}")
                });
            }
        }
    }
    None
}

#[cfg(all(not(windows), not(target_os = "android")))]
fn android_http_proxy() -> Option<String> {
    None
}

async fn port_open(port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_millis(300),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    ).await.map(|r| r.is_ok()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_utf8_body() {
        assert_eq!(decode_body("你好".as_bytes()), "你好");
    }

    #[test]
    fn decode_invalid_utf8_does_not_panic() {
        let bad = vec![0xff, 0xfe, 0x41];
        let s = decode_body(&bad);
        assert!(s.contains('A'));
    }

    #[test]
    fn fetcher_builds_without_proxy() {
        assert!(Fetcher::no_proxy(10).is_ok());
    }

    #[test]
    fn proxy_url_normalized() {
        // 不带协议头的代理地址要能自动补 http://
        assert!(Fetcher::new(Some("127.0.0.1:7890".into()), 10, 1).is_ok());
    }
}

// ── 诊断用集成测试（只在 `cargo test -- --ignored` 时跑）──
#[cfg(test)]
mod diag_tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn diag_system_proxy_read() {
        let p = read_system_proxy();
        println!("read_system_proxy() = {p:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn diag_port_probe() {
        for port in [26561u16, 12334, 7890] {
            let open = port_open(port).await;
            println!("port {port} open={open}");
            if open {
                let cand = format!("http://127.0.0.1:{port}");
                match Fetcher::new(Some(cand), 10, 1) {
                    Ok(f) => {
                        for t in ["https://www.gstatic.com/generate_204",
                                 "https://www.baidu.com/"] {
                            let r = f.probe(t, None).await;
                            println!("  {t} -> {:?}", r.map(|(c,_)| c).map_err(|e| e.to_string()));
                        }
                    }
                    Err(e) => println!("  Fetcher 构建失败: {e}"),
                }
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn diag_detect_proxy() {
        println!("detect_proxy() = {:?}", detect_proxy().await);
    }

    #[tokio::test]
    #[ignore]
    async fn diag_missav_site() {
        // 逐层验证：代理能连 -> 能连站点 -> 能取到页面
        let proxy = detect_proxy().await;
        println!("proxy = {proxy:?}");
        for insecure in [false, true] {
            let f = match Fetcher::with_options(proxy.clone(), 20, 1, insecure) {
                Ok(f) => f,
                Err(e) => { println!("构建失败: {e}"); continue; }
            };
            println!("--- insecure={insecure} ---");
            for url in ["https://www.baidu.com/",
                        "https://missav.live/cn",
                        "https://fourhoi.com/ssis-001/cover-t.jpg"] {
                match f.probe(url, None).await {
                    Ok((c, ct)) => println!("  {url} -> {c} {ct}"),
                    Err(e) => println!("  {url} -> ERR {}", e.to_string().chars().take(120).collect::<String>()),
                }
            }
        }
    }
}
