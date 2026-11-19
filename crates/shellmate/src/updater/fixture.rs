use super::{Phase, Updater};
use crate::update::{DOWNLOAD_PAGE, release::Release};
use gpui::App;
use serde_json::json;

pub fn scene(name: &str, cx: &mut App) {
    let filename = if cfg!(target_arch = "aarch64") {
        "Shellmate-0.1.9-arm64.dmg"
    } else {
        "Shellmate-0.1.9-universal.dmg"
    };
    let size = 22_800_000;
    let data = json!({
        "tag_name": "v0.1.9",
        "name": "Shellmate 0.1.9",
        "body": "- 支持在应用内检查和安装更新\n- 改进多个会话之间的切换\n- 优化文件传输进度显示\n- 修复连接恢复时的显示问题",
        "html_url": DOWNLOAD_PAGE,
        "published_at": "2026-11-20T02:20:00Z",
        "assets": [{
            "name": filename,
            "size": size,
            "browser_download_url": DOWNLOAD_PAGE,
            "digest": format!("sha256:{}", "ab".repeat(32)),
        }],
    });
    let s = cx.global_mut::<Updater>();
    s.release = Some(
        Release::parse(&data.to_string())
            .expect("fixture release")
            .into(),
    );
    s.prefs.last = Some(crate::proto::timestamp("11-12 10:20"));
    s.phase = match name {
        "update-ready" => Phase::Ready,
        "update-downloading" => Phase::Downloading(size * 42 / 100, size),
        "update-error" => Phase::Error("安装包 SHA-256 校验失败".into()),
        "update-fallback" => Phase::Fallback("应用位于只读卷，请前往下载页。".into()),
        "update-uptodate" => {
            s.release = None;
            Phase::Current
        }
        _ => Phase::Available,
    };
}
