//! Shellmate — a gpui SSH client. P0/P1: UI skeleton and local terminal; P2: host library, SSH.

mod security;
mod ai;
mod app_icon;
mod assets;
mod icons;
#[cfg(debug_assertions)]
mod proto;
mod connect;
mod dialogs;
mod editor;
mod files;
mod finder;
mod forward;
mod library;
mod layout;
mod motion;
mod tabs;
mod monitor;
mod productivity;
mod reconnect;
#[cfg(debug_assertions)]
mod demo;
mod runtime;
mod sidebar;
mod theme;
mod ui;
mod workspace;
mod settings;
mod records;
mod menus;
mod update;
mod updater;

use std::borrow::Cow;

use gpui::{
    App, AppContext as _, Bounds, TitlebarOptions, WindowAppearance, WindowBounds,
    WindowOptions, point, px, size,
};

use shellmate_store::HostStore;
use workspace::Workspace;

const FONTS: [&[u8]; 4] = [
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Bold.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Italic.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-BoldItalic.ttf"),
];

/// Opens the host library: `$SHELLMATE_DB` if set (handy for trying the app with a scratch
/// database), else `~/Library/Application Support/Shellmate/shellmate.db`. If that fails the
/// app runs on a temporary in-memory library and returns the reason to show to the user.
fn open_store() -> (HostStore, Option<String>) {
    #[cfg(debug_assertions)] if proto::requested() || std::env::var("SHELLMATE_DEMO").is_ok_and(|s| s.starts_with("p6:")) {return (HostStore::open_in_memory().expect("fixture database"),None);}
    let path = std::env::var_os("SHELLMATE_DB").map(std::path::PathBuf::from).or_else(HostStore::default_path);
    let opened = match &path {
        Some(path) => HostStore::open(path),
        None => Err(shellmate_store::StoreError::Corrupt("找不到用户目录".into())),
    };
    match opened {
        Ok(store) => (store, None),
        Err(err) => {
            log::error!("failed to open the host library: {err}");
            let store = HostStore::open_in_memory().expect("an in-memory SQLite database always opens");
            (store, Some(format!("{err}。本次运行使用临时主机库，新建的主机不会被保存。")))
        }
    }
}

pub(crate) fn prototype_fixture()->bool {
    #[cfg(debug_assertions)] {proto::requested()}
    #[cfg(not(debug_assertions))] {false}
}

fn init_actions(cx: &mut App) {
    menus::init(cx);
    gpui::init(cx);
    shellmate_term::init(cx);
    sidebar::init(cx);
    workspace::init(cx);
    files::panel::init(cx);
    editor::view::init(cx);
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    #[cfg(target_os = "macos")]
    menus::configure_appkit();
    let app = gpui::application().with_assets(assets::AppAssets);
    let (url_sender, url_receiver) = async_channel::unbounded();
    #[cfg(debug_assertions)]
    let debug_sender = url_sender.clone();
    app.on_open_urls(move |urls| {
        if let Err(err) = url_sender.try_send(urls) {
            log::warn!("open url: workspace unavailable: {err}");
        }
    });
    app.on_reopen(workspace::Hub::reopen);
    app.run(move |cx: &mut App| {
        init_actions(cx);
        if let Err(err) = cx.text_system().add_fonts(FONTS.iter().map(|f| Cow::Borrowed(*f)).collect()) {
            log::error!("failed to load bundled fonts: {err:#}");
        }
        runtime::init(cx);

        let dark = matches!(cx.window_appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark);
        app_icon::apply(dark);
        #[cfg(debug_assertions)]
        let dark = match std::env::var("SHELLMATE_THEME").as_deref() {
            Ok("dark") => true,
            Ok("light") => false,
            _ => dark,
        };
        theme::apply(dark, cx);

        let (store, store_notice) = open_store();
        workspace::Hub::init(store, store_notice.is_some(), cx);
        let (main_window, workspace) = open_workspace(true, store_notice, cx).expect("failed to open the main window");
        #[cfg(not(debug_assertions))]
        let _ = (main_window, workspace);
        workspace::open_urls::listen(url_receiver, cx);
        #[cfg(debug_assertions)]
        workspace::open_urls::debug_hook(debug_sender, cx);
        menus::init_hidden_shortcuts(cx);
        #[cfg(debug_assertions)]
        workspace::lifecycle::debug_hooks(cx);
        #[cfg(debug_assertions)]
        if let Ok(name) = std::env::var("SHELLMATE_DEMO") {
            main_window.update(cx, |_, window, cx| demo::run(&name, &workspace, window, cx)).ok();
        }
        cx.activate(true);
        #[cfg(debug_assertions)]
        if let Some(path) = std::env::var_os("SHELLMATE_MENU_DUMP") {
            main_window.update(cx, |_, window, _| menus::dump::after_frame(path.into(), window)).ok();
        }
    });
}

fn open_workspace(first: bool, store_notice: Option<String>, cx: &mut App) -> anyhow::Result<(gpui::AnyWindowHandle, gpui::Entity<Workspace>)> {
    let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
    #[cfg(debug_assertions)]
    let bounds = if proto::requested() {
        let (width, height) = proto::window_size();
        Bounds::new(point(px(60.), px(60.)), size(px(width as f32), px(height as f32)))
    } else { bounds };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("Shellmate".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(12.), px(21.))),
        }),
        app_owns_titlebar_drag: true,
        window_min_size: Some(size(px(980.), px(620.))),
        ..Default::default()
    };
    let (main_window, workspace) = gpui::open_window(options, cx, move |window, cx| {
        // The Dock icon follows the system appearance, like macOS's own dark icons.
        window
            .observe_window_appearance(|window, _| {
                app_icon::apply(app_icon::is_dark(window.appearance()))
            })
            .detach();
        cx.new(|cx| Workspace::new(store_notice, first, window, cx))
    })
    ?;
    workspace::Hub::install(main_window, &workspace, cx);
    Ok((main_window, workspace))
}
