//! Shellmate — a gpui SSH client. P0/P1: UI skeleton and local terminal.

mod assets;
mod sidebar;
mod theme;
mod ui;
mod workspace;

use std::borrow::Cow;

use gpui::{
    App, AppContext as _, Bounds, Menu, MenuItem, TitlebarOptions, WindowAppearance, WindowBounds,
    WindowOptions, point, px, size,
};

use workspace::{Quit, Workspace};

const FONTS: [&[u8]; 4] = [
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Bold.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Italic.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-BoldItalic.ttf"),
];

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    gpui::application().with_assets(assets::AppAssets).run(|cx: &mut App| {
        gpui::init(cx);
        if let Err(err) = cx.text_system().add_fonts(FONTS.iter().map(|f| Cow::Borrowed(*f)).collect()) {
            log::error!("failed to load bundled fonts: {err:#}");
        }
        shellmate_term::init(cx);
        workspace::init(cx);

        let dark = matches!(cx.window_appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark);
        theme::apply(dark, cx);

        cx.set_menus(vec![Menu::new("Shellmate").items(vec![MenuItem::action("退出 Shellmate", Quit)])]);
        cx.on_window_closed(|cx, _| cx.quit()).detach();

        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Shellmate".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(18.), px(19.))),
            }),
            app_owns_titlebar_drag: true,
            window_min_size: Some(size(px(980.), px(620.))),
            ..Default::default()
        };
        gpui::open_window(options, cx, |window, cx| cx.new(|cx| Workspace::new(window, cx)))
            .expect("failed to open the main window");
        cx.activate(true);
    });
}
