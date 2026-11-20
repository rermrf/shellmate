//! One presentation model for the menu, toolbar and every update/settings dialog.
//! Storage and view updates stay on GPUI's thread. HTTP and staging run on the SSH runtime.
mod dialog;
#[cfg(debug_assertions)]
mod fixture;
mod preferences;
pub use dialog::{manual, open, settings};
#[cfg(debug_assertions)]
pub use fixture::scene;

use crate::{
    files::format::now_secs,
    update::{
        self,
        check::{self, Cache, Outcome},
        download, install,
        release::Release,
    },
};
use anyhow::{Context as _, Result};
use gpui::{App, Global};
use preferences::Preferences;
use semver::Version;
use shellmate_store::SharedStore;
use std::{sync::Arc, time::Duration};
use tokio::sync::{oneshot, watch};

enum Phase {
    Idle,
    Checking,
    Current,
    Available,
    Downloading(u64, u64),
    Verifying,
    Ready,
    CheckFailed(String),
    Error(String),
    Fallback(String),
}

pub struct Updater {
    prefs: Preferences,
    api: Option<String>,
    cache: Cache,
    release: Option<Arc<Release>>,
    phase: Phase,
    // A manual check joining a scheduled request must receive its errors.
    checking: Option<bool>,
    cancel: Option<oneshot::Sender<()>>,
    staged: Option<install::Staged>,
    at_quit: bool,
}

impl Global for Updater {}

fn api() -> Option<String> {
    if crate::prototype_fixture()
        || std::env::var("SHELLMATE_DEMO").is_ok_and(|name| name.starts_with("proto:"))
    {
        return None;
    }
    #[cfg(debug_assertions)]
    {
        std::env::var("SHELLMATE_UPDATE_API")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }
    #[cfg(not(debug_assertions))]
    {
        Some("https://api.github.com".into())
    }
}

pub fn init(store: SharedStore, cx: &mut App) {
    let api = api();
    let (prefs, cache) = Preferences::load(store, api.as_deref());
    let mut state = Updater {
        prefs,
        cache,
        api,
        release: None,
        phase: Phase::Idle,
        checking: None,
        cancel: None,
        staged: None,
        at_quit: false,
    };
    if let Ok(release) = Release::parse(&state.cache.json) {
        state.accept(release, false);
    }
    let enabled = state.api.is_some();
    cx.set_global(state);
    cx.on_app_quit(|cx| {
        let state = cx.global_mut::<Updater>();
        if let Some(staged) = state.staged.take()
            && state.at_quit
            && let Err(e) = staged.install()
        {
            log::error!("installing update at quit: {e:#}");
        }
        async {}
    })
    .detach();
    if enabled {
        cx.spawn(async |cx| {
            let mut delay = 10;
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(delay))
                    .await;
                cx.update(|cx| check_now(false, cx));
                delay = 24 * 60 * 60;
            }
        })
        .detach();
    }
}

impl Updater {
    fn accept(&mut self, release: Release, manual: bool) {
        let current = Version::parse(update::VERSION).expect("Cargo version");
        if release.newer_than(&current) {
            if !manual && self.prefs.skipped == release.version.to_string() {
                self.release = None;
                self.phase = Phase::Idle;
                return;
            }
            self.phase = match release.asset(cfg!(target_arch = "aarch64")) {
                None => Phase::Fallback("此版本没有适用于本机的安装包，请前往下载页。".into()),
                Some(asset) => match release.checksum(asset) {
                    Ok(_) => Phase::Available,
                    Err(e) => Phase::Fallback(e.to_string()),
                },
            };
            self.release = Some(Arc::new(release));
        } else {
            self.phase = Phase::Current;
            self.release = None;
        }
    }

    fn check_target(&mut self) {
        if matches!(self.phase, Phase::Available)
            && !crate::prototype_fixture()
            && let Err(e) = install::target()
        {
            self.phase = Phase::Fallback(e.to_string());
        }
    }

    fn busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Downloading(..) | Phase::Verifying | Phase::Ready
        )
    }

    pub(crate) fn available_version(&self) -> Option<String> {
        self.release.as_ref().map(|release| release.version.to_string())
    }

    pub fn pill(&self) -> Option<String> {
        let version = self.available_version()?;
        if self.prefs.skipped == version {
            return None;
        }
        Some(match self.phase {
            Phase::Downloading(n, total) => format!("下载中 {}%", percent(n, total)),
            Phase::Verifying => "下载中 100%".into(),
            Phase::Ready => "重启以更新".into(),
            _ => format!("新版本 {version}"),
        })
    }
}

fn percent(n: u64, total: u64) -> u64 {
    n.saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100)
}

fn check_now(manual: bool, cx: &mut App) {
    let state = cx.global_mut::<Updater>();
    if state.busy() {
        return;
    }
    if !manual && !state.prefs.due(now_secs()) {
        return;
    }
    if let Some(pending) = state.checking.as_mut() {
        if manual {
            *pending = true;
            state.phase = Phase::Checking;
        }
        return;
    }
    let Some(api) = state.api.clone() else {
        if manual {
            state.phase = Phase::Fallback("此构建未启用在线更新，请前往下载页。".into());
        }
        return;
    };
    state.checking = Some(manual);
    if manual {
        state.phase = Phase::Checking;
    }
    let cache = state.cache.clone();
    let Some(rt) = crate::runtime::get(cx) else {
        finish_check(Err(anyhow::anyhow!("更新服务暂不可用")), cx);
        return;
    };
    let work = rt.spawn(async move { check::latest(&check::client()?, &api, &cache).await });
    cx.spawn(async move |cx| {
        let result = work.await.context("更新服务中断").and_then(|r| r);
        cx.update(|cx| finish_check(result, cx));
    })
    .detach();
}

fn finish_check(result: Result<Outcome>, cx: &mut App) {
    let s = cx.global_mut::<Updater>();
    let manual = s.checking.take().unwrap_or(false);
    let result = result.and_then(|outcome| match outcome {
        Outcome::RateLimited => Ok(None),
        Outcome::Same => Ok(Some(Release::parse(&s.cache.json)?)),
        Outcome::Release(release, cache) => {
            s.cache = cache;
            Ok(Some(*release))
        }
    });
    match result {
        Ok(Some(release)) => {
            s.prefs.save_check(s.api.as_deref(), &s.cache, now_secs());
            if !s.busy() {
                s.accept(release, manual);
                if manual {
                    s.check_target();
                }
            }
        }
        Ok(None) if manual => s.phase = Phase::CheckFailed(check::RATE_LIMIT.into()),
        Err(e) => {
            log::warn!("checking for update: {e:#}");
            if manual {
                s.phase = Phase::CheckFailed(e.to_string());
            }
        }
        _ => {}
    }
    cx.refresh_windows();
}

fn start_download(cx: &mut App) {
    if crate::prototype_fixture() {
        return;
    }
    let s = cx.global_mut::<Updater>();
    if s.busy() {
        return;
    }
    let Some(release) = s.release.clone() else {
        return;
    };
    let prepared = (|| {
        anyhow::ensure!(s.api.is_some(), "此构建未启用在线更新，请前往下载页。");
        let asset = release
            .asset(cfg!(target_arch = "aarch64"))
            .context("此版本没有适用于本机的安装包")?
            .clone();
        let source = release.checksum(&asset)?;
        let (bundle, requirement) = install::target()?;
        Ok((asset, source, requirement, bundle))
    })();
    let (asset, source, requirement, bundle) = match prepared {
        Ok(v) => v,
        Err(e) => {
            s.phase = Phase::Fallback(format!("{e}"));
            cx.refresh_windows();
            return;
        }
    };
    let size = asset.size;
    let (stop, cancel) = oneshot::channel();
    // Coalesce progress events when the UI is between frames.
    let (progress, mut events) = watch::channel(Some((0, size)));
    s.cancel = Some(stop);
    s.phase = Phase::Downloading(0, size);
    let Some(rt) = crate::runtime::get(cx) else {
        cx.global_mut::<Updater>().phase = Phase::Error("更新服务暂不可用".into());
        cx.refresh_windows();
        return;
    };
    let work = rt.spawn(async move {
        let path = download::download(
            &check::client()?,
            &asset,
            source,
            &download::directory()?,
            cancel,
            |n, total| {
                progress.send_replace(Some((n, total)));
            },
        )
        .await?;
        let Some(path) = path else { return Ok(None) };
        progress.send_replace(None);
        tokio::task::spawn_blocking(move || {
            install::stage(&path, bundle, release.version.clone(), requirement)
        })
        .await
        .context("校验服务中断")?
        .map(Some)
    });
    cx.spawn(async move |cx| {
        while events.changed().await.is_ok() {
            cx.update(|cx| {
                let s = cx.global_mut::<Updater>();
                match *events.borrow_and_update() {
                    Some((n, total)) => s.phase = Phase::Downloading(n, total),
                    None => {
                        s.phase = Phase::Verifying;
                        s.cancel = None;
                    }
                }
                cx.refresh_windows();
            });
        }
        let result = work.await.context("下载服务中断").and_then(|r| r);
        cx.update(|cx| {
            let s = cx.global_mut::<Updater>();
            s.cancel = None;
            s.phase = match result {
                Ok(Some(staged)) => {
                    s.staged = Some(staged);
                    Phase::Ready
                }
                Ok(None) => Phase::Available,
                Err(e) => {
                    log::warn!("preparing update: {e:#}");
                    Phase::Error(e.to_string())
                }
            };
            cx.refresh_windows();
        });
    })
    .detach();
    cx.refresh_windows();
}

fn restart(cx: &mut App) {
    let s = cx.global_mut::<Updater>();
    let Some(staged) = s.staged.take() else {
        return;
    };
    s.at_quit = false;
    if let Err(e) = staged.install() {
        s.phase = Phase::Error(e.to_string());
    } else if let Err(e) = install::relaunch(&staged.bundle) {
        s.phase = Phase::Fallback(e.to_string());
    } else {
        crate::workspace::Hub::quit(cx);
    }
    cx.refresh_windows();
}
