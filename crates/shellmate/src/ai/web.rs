//! Public web access shared by the parent and its children. No terminal channel.
#[path = "web_html.rs"]
mod html;
use futures::future::BoxFuture;
use shellmate_ai::web::{Fetch, Hit, Request, ResultData, Search};
use shellmate_store::{SecretKey, SecretStore, SharedStore};
use std::{
    collections::VecDeque,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
pub const ENABLED: &str = "ai_web_enabled";
pub const FALLBACK: &str = "ai_web_duckduckgo";
pub const KEY: &str = "ai:web:tavily_api_key";
const MAX_BYTES: usize = 2 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(600);
#[derive(Clone)]
pub(super) struct Response {
    status: u16,
    url: String,
    content_type: String,
    body: String,
}
pub(super) trait Transport: Send + Sync {
    fn request<'a>(
        &'a self,
        url: &'a str,
        body: Option<Vec<u8>>,
        key: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Response, String>>;
}
struct Network;
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && !ip.is_documentation()
                && a != 0
                && a < 224
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 198 && (b == 18 || b == 19))
                && !(a == 192 && b == 0 && c == 0)
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && s[0] != 0x2002
        }
    }
}
pub(super) fn public_url(text: &str) -> Result<reqwest::Url, String> {
    let mut u = reqwest::Url::parse(text).map_err(|_| "URL 无效，请提供完整 HTTP(S) 地址")?;
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some()
    {
        return Err("仅支持不含登录凭据的公开 HTTP(S) 地址".into());
    }
    let host = u.host_str().ok_or("URL 缺少域名")?.trim_matches(['[', ']']);
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || (!host.contains('.') && !host.contains(':'))
    {
        return Err("网页读取仅支持公网地址".into());
    }
    if let Ok(ip) = host.parse::<IpAddr>()
        && !public_ip(ip)
    {
        return Err("网页读取仅支持公网地址".into());
    }
    u.set_fragment(None);
    Ok(u)
}
struct PublicDns;
impl reqwest::dns::Resolve for PublicDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addresses: Vec<_> = tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
                return Err(std::io::Error::other("目标不是公网地址").into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}
impl Transport for Network {
    fn request<'a>(
        &'a self,
        url: &'a str,
        body: Option<Vec<u8>>,
        key: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Response, String>> {
        Box::pin(async move {
            let mut url = public_url(url)?;
            let client = reqwest::Client::builder()
                .no_proxy()
                .dns_resolver(Arc::new(PublicDns))
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(25))
                .user_agent("Shellmate/0.3 (public web reader)")
                .build()
                .map_err(|_| "无法创建网络连接")?;
            for hop in 0..=4 {
                let mut request = if let Some(data) = &body {
                    client
                        .post(url.clone())
                        .header("Content-Type", "application/json")
                        .body(data.clone())
                } else {
                    client.get(url.clone())
                };
                if let Some(key) = key {
                    request = request.bearer_auth(key);
                }
                let mut response = request.send().await.map_err(|e| {
                    if e.is_timeout() {
                        "网页请求超时"
                    } else {
                        "网页请求失败，请检查网络或目标地址"
                    }
                })?;
                if response.status().is_redirection() {
                    if key.is_some() || body.is_some() {
                        return Err("搜索服务返回了非预期跳转".into());
                    }
                    if hop == 4 {
                        return Err("网页跳转次数过多".into());
                    }
                    let location = response
                        .headers()
                        .get("location")
                        .and_then(|v| v.to_str().ok())
                        .ok_or("网页跳转缺少地址")?;
                    url = public_url(url.join(location).map_err(|_| "网页跳转地址无效")?.as_str())?;
                    continue;
                }
                let status = response.status().as_u16();
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("text/html")
                    .to_owned();
                if response
                    .content_length()
                    .is_some_and(|n| n > MAX_BYTES as u64)
                {
                    return Err("网页超过 2 MiB 读取上限".into());
                }
                let mut bytes = vec![];
                while let Some(chunk) = response.chunk().await.map_err(|_| "读取网页响应失败")?
                {
                    if bytes.len() + chunk.len() > MAX_BYTES {
                        return Err("网页超过 2 MiB 读取上限".into());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let encoding = content_type
                    .split(';')
                    .find_map(|v| v.trim().strip_prefix("charset="))
                    .and_then(|s| encoding_rs::Encoding::for_label(s.trim_matches('"').as_bytes()))
                    .unwrap_or(encoding_rs::UTF_8);
                let text = encoding.decode(&bytes).0.into_owned();
                return Ok(Response {
                    status,
                    url: url.into(),
                    content_type,
                    body: text,
                });
            }
            Err("网页读取失败".into())
        })
    }
}
#[derive(Clone)]
enum Cached {
    Search(ResultData),
    Page {
        url: String,
        title: String,
        content: String,
        at: i64,
    },
}
pub struct Service {
    db: SharedStore,
    credentials: Arc<dyn SecretStore>,
    transport: Arc<dyn Transport>,
    slots: tokio::sync::Semaphore,
    serial: tokio::sync::Mutex<()>,
    cache: Mutex<VecDeque<(String, Instant, Cached)>>,
}
impl Service {
    pub fn new(db: SharedStore, credentials: Arc<dyn SecretStore>) -> Self {
        Self {
            db,
            credentials,
            transport: Arc::new(Network),
            slots: tokio::sync::Semaphore::new(3),
            serial: Default::default(),
            cache: Default::default(),
        }
    }
    pub fn enabled(&self) -> bool {
        self.db.borrow().setting(ENABLED).ok().flatten().as_deref() == Some("1")
    }
    fn cached(&self, key: &str) -> Option<Cached> {
        self.cache
            .lock()
            .unwrap()
            .iter()
            .find(|(k, at, _)| k == key && at.elapsed() < TTL)
            .map(|(_, _, v)| v.clone())
    }
    fn cache(&self, key: String, value: Cached) {
        let mut c = self.cache.lock().unwrap();
        c.retain(|(k, at, _)| k != &key && at.elapsed() < TTL);
        while c.len() >= 16 {
            c.pop_front();
        }
        c.push_back((key, Instant::now(), value));
    }
    pub fn page_text(&self, url: &str) -> Option<String> {
        self.cache
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|(_, _, c)| {
                if let Cached::Page {
                    url: u, content, ..
                } = c
                    && u == url
                {
                    Some(content.clone())
                } else {
                    None
                }
            })
    }
    pub async fn execute(&self, request: Request) -> Result<ResultData, String> {
        if !self.enabled() {
            return Err("网页工具已关闭".into());
        }
        let _slot = self.slots.acquire().await.map_err(|_| "网页服务已关闭")?;
        tokio::time::timeout(Duration::from_secs(55), async {
            match request {
                Request::Search(s) => self.search(s).await,
                Request::Fetch(f) => self.fetch(f).await,
            }
        })
        .await
        .map_err(|_| "网页工具超时，请稍后重试")?
    }
    async fn search(&self, args: Search) -> Result<ResultData, String> {
        let fallback = self.db.borrow().setting(FALLBACK).ok().flatten().as_deref() != Some("0");
        let cache_key = format!(
            "search:{fallback}:{}",
            serde_json::to_string(&args).unwrap()
        );
        let _serial = self.serial.lock().await;
        if let Some(Cached::Search(mut data)) = self.cached(&cache_key) {
            if let ResultData::Search { cached, .. } = &mut data {
                *cached = true;
            }
            return Ok(data);
        }
        let credentials = self.credentials.clone();
        let key =
            tokio::task::spawn_blocking(move || credentials.get(&SecretKey::Account(KEY.into())))
                .await
                .map_err(|_| "无法读取搜索密钥")?
                .map_err(|_| "无法读取搜索密钥，请解锁凭据存储后重试")?;
        let Some(key) = key.filter(|k| !k.is_empty()) else {
            return Err("请在 AI 设置中保存 Tavily API Key 后搜索".into());
        };
        let payload = serde_json::json!({"query":args.query,"max_results":args.max_results,"include_domains":args.include_domains,"search_depth":"basic","auto_parameters":false,"include_answer":false,"include_raw_content":false});
        let primary = self
            .transport
            .request(
                "https://api.tavily.com/search",
                Some(serde_json::to_vec(&payload).unwrap()),
                Some(key.expose()),
            )
            .await;
        let (results, provider, note) = match primary {
            Ok(r) if r.status == 200 => {
                let value: serde_json::Value =
                    serde_json::from_str(&r.body).map_err(|_| "Tavily 返回格式异常")?;
                let items = value["results"]
                    .as_array()
                    .ok_or("Tavily 返回缺少搜索结果")?;
                let hits = items
                    .iter()
                    .filter_map(|i| {
                        let url = public_url(i["url"].as_str()?).ok()?;
                        Some(Hit {
                            url: url.into(),
                            title: trim(i["title"].as_str().unwrap_or("网页"), 300),
                            snippet: trim(i["content"].as_str().unwrap_or_default(), 2000),
                        })
                    })
                    .filter(|h| domain_allowed(&h.url, &args.include_domains))
                    .take(args.max_results)
                    .collect();
                (hits, "Tavily", None)
            }
            Ok(r) if matches!(r.status, 401 | 403) => {
                return Err("Tavily API Key 无效或无权限，请检查设置".into());
            }
            Ok(r) if matches!(r.status, 429 | 432 | 433 | 500..=599) && fallback => (
                self.duck(&args).await?,
                "DuckDuckGo",
                Some("Tavily 暂不可用或额度受限，已使用备用搜索".into()),
            ),
            Err(_) if fallback => (
                self.duck(&args).await?,
                "DuckDuckGo",
                Some("Tavily 网络请求失败，已使用备用搜索".into()),
            ),
            Ok(r) => return Err(format!("Tavily 搜索失败（HTTP {}）", r.status)),
            Err(e) => return Err(e),
        };
        let data = ResultData::Search {
            query: args.query,
            provider: provider.into(),
            results,
            searched_at: crate::ai::now(),
            cached: false,
            note,
        };
        self.cache(cache_key, Cached::Search(data.clone()));
        Ok(data)
    }
    async fn duck(&self, args: &Search) -> Result<Vec<Hit>, String> {
        let query = if args.include_domains.is_empty() {
            args.query.clone()
        } else {
            format!(
                "{} ({})",
                args.query,
                args.include_domains
                    .iter()
                    .map(|d| format!("site:{d}"))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            )
        };
        let mut url = reqwest::Url::parse("https://lite.duckduckgo.com/lite/").unwrap();
        url.query_pairs_mut().append_pair("q", &query);
        let r = self.transport.request(url.as_str(), None, None).await?;
        if r.status != 200 || html::challenge(&r.body) {
            return Err("DuckDuckGo 备用搜索被限流或需要验证，请稍后重试".into());
        }
        Ok(html::duck(&r.body)?
            .into_iter()
            .filter(|h| domain_allowed(&h.url, &args.include_domains))
            .take(args.max_results)
            .collect())
    }
    async fn fetch(&self, args: Fetch) -> Result<ResultData, String> {
        let url = public_url(&args.url)?.to_string();
        let key = format!("page:{url}");
        let cached = self.cached(&key).or_else(|| {
            self.cache
                .lock()
                .unwrap()
                .iter()
                .find_map(|(_, at, c)| match c {
                    Cached::Page { url: u, .. } if u == &url && at.elapsed() < TTL => {
                        Some(c.clone())
                    }
                    _ => None,
                })
        });
        if args.offset > 0 && cached.is_none() {
            return Err("正文缓存已过期，请从 offset=0 重新读取，避免拼接不同版本".into());
        }
        let from_cache = cached.is_some();
        let page = if let Some(page) = cached {
            page
        } else {
            let r = self.transport.request(&url, None, None).await?;
            if r.status != 200 {
                return Err(format!("网页读取失败（HTTP {}）", r.status));
            }
            if html::challenge(&r.body) {
                return Err("网页需要登录或人机验证，未读取到正文".into());
            }
            let is_html = r.content_type.contains("html");
            let (title, content) = if is_html {
                html::extract(&r.body, &r.url)?
            } else if r.content_type.starts_with("text/")
                || r.content_type.contains("json")
                || r.content_type.contains("xml")
            {
                (r.url.clone(), r.body)
            } else {
                return Err("当前支持 HTML、文本、Markdown、JSON 和 XML；不读取二进制文件".into());
            };
            if content.trim().is_empty() || (is_html && content.trim().len() < 40) {
                return Err("未提取到有效正文，网页可能需要 JavaScript 或登录".into());
            }
            let page = Cached::Page {
                url: r.url,
                title,
                content,
                at: crate::ai::now(),
            };
            self.cache(key, page.clone());
            page
        };
        let Cached::Page {
            url,
            title,
            content,
            at,
        } = page
        else {
            return Err("网页缓存无效".into());
        };
        let total_chars = content.chars().count();
        if args.offset >= total_chars {
            return Err("正文 offset 超出范围，请从 0 重新读取".into());
        }
        let text: String = content
            .chars()
            .skip(args.offset)
            .take(args.max_chars)
            .collect();
        let next = args.offset + text.chars().count();
        Ok(ResultData::Page {
            url,
            title,
            content: text,
            total_chars,
            offset: args.offset,
            next_offset: (next < total_chars).then_some(next),
            fetched_at: at,
            cached: from_cache,
        })
    }
}
fn trim(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
fn domain_allowed(url: &str, domains: &[String]) -> bool {
    domains.is_empty()
        || reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .is_some_and(|host| {
                domains
                    .iter()
                    .any(|d| host == *d || host.ends_with(&format!(".{d}")))
            })
}
#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
