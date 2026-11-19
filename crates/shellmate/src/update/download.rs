use anyhow::{Context, Result, ensure};
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
};

use super::release::{Asset, Checksum, checksum_file};

pub fn directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("SHELLMATE_CACHE_DIR") {
        return Ok(path.into());
    }
    Ok(std::env::home_dir()
        .context("找不到用户缓存目录")?
        .join("Library/Caches/Shellmate/Updates"))
}

pub async fn checksum(client: &Client, source: Checksum, name: &str) -> Result<[u8; 32]> {
    match source {
        Checksum::Digest(hash) => Ok(hash),
        Checksum::File(url) => {
            let mut response = client
                .get(url)
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .context("无法下载校验文件")?
                .error_for_status()
                .context("无法下载校验文件")?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.context("读取校验文件失败")? {
                ensure!(bytes.len() + chunk.len() <= 4096, "校验文件过大");
                bytes.extend_from_slice(&chunk);
            }
            checksum_file(
                std::str::from_utf8(&bytes).context("校验文件格式无效")?,
                name,
            )
        }
    }
}

struct Partial(PathBuf);
impl Drop for Partial {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

async fn valid(path: &Path, hash: &[u8; 32], size: u64) -> Result<bool> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(false);
    };
    if !metadata.is_file() || metadata.len() != size {
        return Ok(false);
    }
    let mut file = tokio::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let len = file.read(&mut buf).await?;
        if len == 0 {
            break;
        }
        digest.update(&buf[..len]);
    }
    Ok(&<[u8; 32]>::from(digest.finalize()) == hash)
}

/// Dropping the transfer future (including a stalled HTTP read) releases the lock and
/// unlinks the partial. Only a fully flushed, digest-checked file becomes reusable.
pub async fn download(
    client: &Client,
    asset: &Asset,
    source: Checksum,
    dir: &Path,
    cancel: oneshot::Receiver<()>,
    progress: impl FnMut(u64, u64),
) -> Result<Option<PathBuf>> {
    tokio::select! {
        biased;
        _ = cancel => Ok(None),
        result = transfer(client, asset, source, dir, progress) => result.map(Some)
    }
}

async fn transfer(
    client: &Client,
    asset: &Asset,
    source: Checksum,
    dir: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    ensure!(
        Path::new(&asset.name).file_name() == Some(asset.name.as_ref()),
        "安装包名称无效"
    );
    let expected = checksum(client, source, &asset.name).await?;
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .context("无法创建更新缓存目录")?;
    let lock = fs::File::open(dir).context("无法打开更新缓存目录")?;
    lock.try_lock().context("更新缓存正在使用")?;
    let path = dir.join(&asset.name);
    let part = dir.join(format!("{}.part", asset.name));
    remove_if_present(&part).context("无法清理上次的下载")?;
    if valid(&path, &expected, asset.size)
        .await
        .context("无法读取已下载的安装包")?
    {
        progress(asset.size, asset.size);
        return Ok(path);
    }
    remove_if_present(&path).context("无法清理损坏的安装包")?;
    let file = fs::File::create_new(&part).context("无法创建临时下载文件")?;
    let partial = Partial(part);
    let mut file = tokio::fs::File::from_std(file);
    let mut response = client
        .get(&asset.url)
        .send()
        .await
        .context("无法连接下载地址")?
        .error_for_status()
        .context("下载安装包失败")?;
    let mut digest = Sha256::new();
    let mut bytes = 0;
    progress(0, asset.size);
    while let Some(chunk) = response.chunk().await.context("下载中断，请重试")? {
        bytes += chunk.len() as u64;
        ensure!(bytes <= asset.size, "安装包大小与发布信息不符");
        file.write_all(&chunk).await.context("写入下载缓存失败")?;
        digest.update(&chunk);
        progress(bytes, asset.size);
    }
    ensure!(bytes == asset.size, "安装包下载不完整");
    ensure!(
        <[u8; 32]>::from(digest.finalize()) == expected,
        "安装包 SHA-256 校验失败"
    );
    file.flush().await.context("写入下载缓存失败")?;
    drop(file);
    fs::rename(&partial.0, &path).context("保存安装包失败")?;
    Ok(path)
}
