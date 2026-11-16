//! Shellmate — a gpui SSH client. P0/P1: UI skeleton and local terminal; P2: host library, SSH.

mod assets;
#[cfg(debug_assertions)]
mod proto;
mod connect;
mod dialogs;
mod editor;
mod files;
mod forward;
mod library;
mod layout;
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

use std::borrow::Cow;

use gpui::{
    App, AppContext as _, Bounds, Menu, MenuItem, TitlebarOptions, WindowAppearance, WindowBounds,
    WindowOptions, point, px, size,
};

use shellmate_store::HostStore;
use workspace::{CloseTab, DuplicateSession, NewLocalTerminal, Quit, Workspace};

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
    #[cfg(debug_assertions)] if proto::requested(){return (HostStore::open_in_memory().expect("fixture database"),None);}
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

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    gpui::application().with_assets(assets::AppAssets).run(|cx: &mut App| {
        gpui::init(cx);
        if let Err(err) = cx.text_system().add_fonts(FONTS.iter().map(|f| Cow::Borrowed(*f)).collect()) {
            log::error!("failed to load bundled fonts: {err:#}");
        }
        shellmate_term::init(cx);
        runtime::init(cx);
        sidebar::init(cx);
        workspace::init(cx);
        files::panel::init(cx);
        editor::view::init(cx);

        let dark = matches!(cx.window_appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark);
        #[cfg(debug_assertions)]
        let dark = match std::env::var("SHELLMATE_THEME").as_deref() {
            Ok("dark") => true,
            Ok("light") => false,
            _ => dark,
        };
        theme::apply(dark, cx);

        cx.set_menus(vec![
            Menu::new("Shellmate").items(vec![MenuItem::action("退出 Shellmate", Quit)]),
            Menu::new("会话").items(vec![
                MenuItem::action("新建本机终端", NewLocalTerminal),
                MenuItem::action("复制当前会话", DuplicateSession),
                MenuItem::separator(),
                MenuItem::action("关闭标签", CloseTab),
            ]),
        ]);
        cx.on_window_closed(|cx, _| cx.quit()).detach();

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
        let (store, store_notice) = open_store();
        let (_window, _workspace) = gpui::open_window(options, cx, move |window, cx| {
            cx.new(|cx| Workspace::new(store, store_notice, window, cx))
        })
        .expect("failed to open the main window");
        #[cfg(debug_assertions)]
        if let Ok(name) = std::env::var("SHELLMATE_DEMO") {
            _window.update(cx, |_, window, cx| demo::run(&name, &_workspace, window, cx)).ok();
        }
        cx.activate(true);
    });
}
