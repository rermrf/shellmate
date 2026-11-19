use anyhow::{Context, Result, anyhow, ensure};
use reqwest::Url;
use semver::Version;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub url: String,
    pub digest: Option<String>,
}

#[derive(Debug)]
pub struct Release {
    pub version: Version,
    pub name: String,
    pub body: String,
    pub page: String,
    pub published: String,
    pub assets: Vec<Asset>,
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("发布信息缺少 {key}"))
}

fn url(value: &Value, key: &str) -> Result<String> {
    let url = Url::parse(string(value, key)?).context("发布链接无效")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none(),
        "发布链接无效"
    );
    Ok(url.to_string())
}

impl Release {
    pub fn parse(json: &str) -> Result<Self> {
        let v: Value = serde_json::from_str(json).context("发布信息格式无效")?;
        let tag = string(&v, "tag_name")?;
        let version =
            Version::parse(tag.strip_prefix('v').unwrap_or(tag)).context("发布版本号无效")?;
        let assets = v["assets"]
            .as_array()
            .context("发布文件列表无效")?
            .iter()
            .map(|a| {
                Ok(Asset {
                    name: string(a, "name")?.into(),
                    size: a["size"].as_u64().context("文件大小无效")?,
                    url: url(a, "browser_download_url")?,
                    digest: if a["digest"].is_null() {
                        None
                    } else {
                        Some(string(a, "digest")?.into())
                    },
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            version,
            name: v["name"].as_str().unwrap_or("").into(),
            body: v["body"].as_str().unwrap_or("").into(),
            page: url(&v, "html_url")?,
            published: string(&v, "published_at")?.into(),
            assets,
        })
    }

    pub fn newer_than(&self, current: &Version) -> bool {
        self.version.pre.is_empty() && self.version.cmp_precedence(current).is_gt()
    }

    pub fn asset(&self, arm: bool) -> Option<&Asset> {
        let find = |arch| {
            let name = format!("Shellmate-{}-{arch}.dmg", self.version);
            self.assets.iter().find(|a| a.name == name)
        };
        if arm && let Some(asset) = find("arm64") {
            return Some(asset);
        }
        find("universal")
    }

    pub fn checksum(&self, asset: &Asset) -> Result<Checksum> {
        if let Some(digest) = &asset.digest {
            return Ok(Checksum::Digest(parse_hash(
                digest
                    .strip_prefix("sha256:")
                    .context("安装包摘要格式无效")?,
            )?));
        }
        let name = format!("{}.sha256", asset.name);
        self.assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| Checksum::File(a.url.clone()))
            .ok_or_else(|| anyhow!("此安装包缺少 SHA-256 校验值，请前往下载页。"))
    }
}

#[derive(Debug)]
pub enum Checksum {
    Digest([u8; 32]),
    File(String),
}

fn parse_hash(s: &str) -> Result<[u8; 32]> {
    let mut digest = [0; 32];
    hex::decode_to_slice(s, &mut digest).context("SHA-256 校验值无效")?;
    Ok(digest)
}

pub fn checksum_file(text: &str, name: &str) -> Result<[u8; 32]> {
    let (hash, filename) = text
        .trim_end_matches(['\r', '\n'])
        .split_once("  ")
        .context("校验文件格式无效")?;
    ensure!(filename == name, "校验文件与安装包名称不符");
    parse_hash(hash)
}
