use super::release::Release;
use anyhow::{Context, Result};
use reqwest::{Client, StatusCode, header};
use std::time::Duration;

#[derive(Clone, Default)]
pub struct Cache {
    pub etag: Option<String>,
    pub json: String,
}

pub const RATE_LIMIT: &str = "GitHub 暂时限制了查询次数，请稍后再试";

pub enum Outcome {
    Same,
    Release(Box<Release>, Cache),
    RateLimited,
}

pub fn client() -> Result<Client> {
    let builder = Client::builder();
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(20))
        .user_agent(concat!("Shellmate/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("无法初始化更新服务")
}

pub async fn latest(client: &Client, api: &str, cache: &Cache) -> Result<Outcome> {
    let url = format!(
        "{}/repos/rermrf/shellmate/releases/latest",
        api.trim_end_matches('/')
    );
    let mut req = client
        .get(url)
        .timeout(Duration::from_secs(20))
        .header(header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(etag) = &cache.etag {
        req = req.header(header::IF_NONE_MATCH, etag);
    }
    let response = req.send().await.context("无法连接 GitHub，请稍后重试")?;
    if response.status() == StatusCode::NOT_MODIFIED {
        anyhow::ensure!(
            cache.etag.is_some() && !cache.json.is_empty(),
            "本地发布缓存无效"
        );
        return Ok(Outcome::Same);
    }
    if matches!(
        response.status(),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    ) && response
        .headers()
        .keys()
        .any(|h| h == header::RETRY_AFTER || h.as_str().starts_with("x-ratelimit-"))
    {
        return Ok(Outcome::RateLimited);
    }
    let response = response
        .error_for_status()
        .context("GitHub 查询失败，请稍后重试")?;
    let etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|s| s.to_str().ok())
        .map(str::to_owned);
    let json = response.text().await.context("未能读取发布信息")?;
    Ok(Outcome::Release(
        Box::new(Release::parse(&json)?),
        Cache { etag, json },
    ))
}
