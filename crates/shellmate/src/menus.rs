use gpui::{Action, App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType, actions};
use gpui::component::input;
use shellmate_term::view as terminal;

use crate::workspace::{CloseTab, DuplicateSession, NewLocalTerminal, NewWindow, NextTab, OpenPalette, Panel,
    PreviousTab, Quit, ToggleSidebar, ToggleTheme};

use crate::workspace::{Workspace, Hub};

actions!(shellmate, [CloseWindow, About, CheckUpdates, Settings, LockVault, HideApp, HideOthers, ShowAll, NewConnection,
    ToggleSplit, ToggleRecording, ToggleBroadcast, ImportHosts, ImportBundle, ExportBundle,
    EditUndo, EditRedo, EditCut, EditCopy, EditPaste, EditSelectAll, ShowTerminal, ShowFiles,
    ShowForward, ShowMonitor, Fullscreen, History, ToggleCommands, ToggleAi,
    Records, Minimize, Zoom, MoveSessionToNewWindow]);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MenuState {
    pub sidebar_open: bool,
    pub panel: Option<Panel>,
    pub split: bool,
    pub recording: bool,
    pub broadcast: bool,
    pub dark: bool,
    pub commands_open: bool,
    pub ai_open: bool,
    /// Credentials live in the master-password vault, the only store the app can lock.
    pub vault_lock: bool,
}

pub(crate) fn menus(state: &MenuState) -> Vec<Menu> {
    use MenuItem as Item;
    vec![
        Menu::new("Shellmate").items(
            [Item::action("关于 Shellmate", About), Item::action("检查更新…", CheckUpdates), Item::Separator, Item::action("设置…", Settings)]
                .into_iter()
                .chain(state.vault_lock.then(|| Item::action("锁定凭据库", LockVault)))
                .chain([
                    Item::Separator, Item::os_submenu("服务", SystemMenuType::Services), Item::Separator,
                    Item::action("隐藏 Shellmate", HideApp), Item::action("隐藏其他", HideOthers),
                    Item::action("全部显示", ShowAll), Item::Separator, Item::action("退出 Shellmate", Quit),
                ]),
        ),
        Menu::new("会话").items([
            Item::action("新建本机终端", NewLocalTerminal), Item::action("新建窗口", NewWindow),
            Item::action("新建连接…", NewConnection),
            Item::action("复制当前会话", DuplicateSession), Item::Separator,
            Item::action("分屏", ToggleSplit).checked(state.split),
            Item::action(if state.recording { "停止录制" } else { "开始录制" }, ToggleRecording),
            Item::action("广播输入", ToggleBroadcast).checked(state.broadcast), Item::Separator,
            Item::action("导入主机…", ImportHosts), Item::action("导入配置包…", ImportBundle),
            Item::action("导出配置包…", ExportBundle), Item::Separator, Item::action("关闭会话", CloseTab),
            Item::action("关闭窗口", CloseWindow),
        ]),
        Menu::new("编辑").items([
            Item::os_action("撤销", EditUndo, OsAction::Undo), Item::os_action("重做", EditRedo, OsAction::Redo),
            Item::Separator, Item::os_action("剪切", EditCut, OsAction::Cut), Item::os_action("拷贝", EditCopy, OsAction::Copy),
            Item::os_action("粘贴", EditPaste, OsAction::Paste), Item::os_action("全选", EditSelectAll, OsAction::SelectAll),
            Item::Separator, Item::action("查找…", terminal::ToggleSearch), Item::action("查找下一个", terminal::SearchUp),
            Item::action("查找上一个", terminal::SearchDown),
        ]),
        Menu::new("显示").items([
            Item::action(if state.sidebar_open { "隐藏侧边栏" } else { "显示侧边栏" }, ToggleSidebar), Item::Separator,
            Item::action("终端", ShowTerminal).checked(state.panel == Some(Panel::Terminal)),
            Item::action("文件", ShowFiles).checked(state.panel == Some(Panel::Files)),
            Item::action("端口转发", ShowForward).checked(state.panel == Some(Panel::Forward)),
            Item::action("监控", ShowMonitor).checked(state.panel == Some(Panel::Monitor)), Item::Separator,
            Item::action("深色外观", ToggleTheme).checked(state.dark), Item::Separator, Item::action("进入全屏幕", Fullscreen),
        ]),
        Menu::new("工具").items([
            Item::action("命令面板…", OpenPalette), Item::action("命令历史…", History),
            Item::action("快捷命令", ToggleCommands).checked(state.commands_open), Item::Separator,
            Item::action("AI 助手", ToggleAi).checked(state.ai_open),
            Item::Separator, Item::action("会话记录…", Records),
        ]),
        Menu::new("窗口").items([
            Item::action("最小化", Minimize), Item::action("缩放", Zoom), Item::Separator,
            Item::action("将会话移到新窗口", MoveSessionToNewWindow), Item::Separator,
            Item::action("上一个会话", PreviousTab), Item::action("下一个会话", NextTab),
        ]),
    ]
}

pub(crate) fn install(tree: Vec<Menu>, cx: &mut App) {
    cx.set_menus(tree);
    #[cfg(target_os = "macos")]
    {
        // GPUI recognizes "Window" by its English name. Register the localized menu with AppKit.
        use objc2_app_kit::NSApplication;
        use objc2_foundation::ns_string;
        let app = NSApplication::sharedApplication(objc2::MainThreadMarker::new().expect("UI thread"));
        let menu = app.mainMenu()
            .and_then(|menu| menu.itemWithTitle(ns_string!("窗口")))
            .and_then(|item| item.submenu());
        app.setWindowsMenu(menu.as_deref());
        for entry in Hub::windows(cx).to_vec() {
            entry.handle.update(cx, |_, window, _| {
                if let Some(native) = crate::workspace::lifecycle::native_window(window) {
                    app.addWindowsItem_title_filename(&native, &native.title(), false);
                }
            }).ok();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Target { Input, Terminal }

fn edit_target(input_available: bool, terminal_available: bool) -> Option<Target> {
    match (input_available, terminal_available) {
        (true, _) => Some(Target::Input),
        (false, true) => Some(Target::Terminal),
        (false, false) => None,
    }
}

/// AppKit has no responder window for an ordered-out workspace. Only its hidden menu
/// uses this app-level action. Visible menus retain GPUI's focus-sensitive validation.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = shellmate, no_json)]
pub(crate) struct ReopenForMenu(pub &'static str);

/// Give the hidden menu the same native key equivalents without binding extra keys in
/// any visible view. AppKit reads the binding by action when constructing its NSMenu.
pub(crate) fn init_hidden_shortcuts(cx: &mut App) {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let mut shortcuts = Vec::new();
    for item in menus(&MenuState::default()).into_iter().flat_map(|menu| menu.items) {
        let MenuItem::Action { action, .. } = item else { continue };
        let Some(binding) = keymap.bindings_for_action(action.as_ref()).next() else { continue };
        let [key] = binding.keystrokes() else { continue };
        let scope = gpui::KeyBindingContextPredicate::parse("HiddenWorkspaceMenu").expect("menu context");
        shortcuts.push(KeyBinding::load(&key.unparse(cx.platform_info().os),
            Box::new(ReopenForMenu(action.name())), Some(scope.into()), false, None, cx)
            .expect("existing menu shortcut"));
    }
    drop(keymap);
    cx.bind_keys(shortcuts);
}

fn forward_edit(cx: &mut App, input: &'static dyn Action, terminal: Option<&'static dyn Action>) {
    Hub::with_restored_window(cx, move |window, cx| {
        let target = edit_target(window.is_action_available(input, cx),
            terminal.is_some_and(|action| window.is_action_available(action, cx)));
        let action = match target {
            Some(Target::Input) => Some(input),
            Some(Target::Terminal) => terminal,
            None => None,
        };
        if let Some(action) = action { window.dispatch_action(action.boxed_clone(), cx); }
    });
}

pub(crate) fn global_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-z", EditUndo, None), KeyBinding::new("cmd-shift-z", EditRedo, None),
        KeyBinding::new("cmd-x", EditCut, None), KeyBinding::new("cmd-c", EditCopy, None),
        KeyBinding::new("cmd-v", EditPaste, None), KeyBinding::new("cmd-a", EditSelectAll, None),
        KeyBinding::new("cmd-h", HideApp, None), KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None), KeyBinding::new("ctrl-cmd-f", Fullscreen, None),
        KeyBinding::new("cmd-q", Quit, None), KeyBinding::new("cmd-shift-w", CloseWindow, None),
    ]
}

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys(global_bindings());
    cx.on_action(|_: &EditUndo, cx| forward_edit(cx, &input::Undo, None))
        .on_action(|_: &EditRedo, cx| forward_edit(cx, &input::Redo, None))
        .on_action(|_: &EditCut, cx| forward_edit(cx, &input::Cut, None))
        .on_action(|_: &EditCopy, cx| forward_edit(cx, &input::Copy, Some(&terminal::Copy)))
        .on_action(|_: &EditPaste, cx| forward_edit(cx, &input::Paste, Some(&terminal::Paste)))
        .on_action(|_: &EditSelectAll, cx| forward_edit(cx, &input::SelectAll, Some(&terminal::SelectAll)))
        .on_action(|_: &CheckUpdates, cx| Hub::with_restored_window(cx, crate::updater::manual))
        .on_action(|_: &About, cx| Hub::with_restored_window(cx, crate::dialogs::about::open))
        .on_action(|_: &HideApp, cx| cx.hide())
        .on_action(|_: &HideOthers, cx| cx.hide_other_apps())
        .on_action(|_: &ShowAll, cx| cx.unhide_other_apps())
        .on_action(|_: &Minimize, cx| Hub::with_restored_window(cx, |window, _| window.minimize_window()))
        .on_action(|_: &Zoom, cx| Hub::with_restored_window(cx, |window, _| window.zoom_window()))
        .on_action(|_: &Fullscreen, cx| Hub::with_restored_window(cx, |window, _| window.toggle_fullscreen()))
        .on_action(|_: &CloseWindow, cx| Hub::update(cx, Workspace::close_window))
        .on_action(|_: &Quit, cx| Hub::update(cx, Workspace::request_quit))
        .on_action(|action: &ReopenForMenu, cx| {
            let name = action.0;
            Hub::update(cx, move |s, w, cx| s.perform_hidden_menu(name, w, cx));
        });
}

#[cfg(target_os = "macos")]
pub(crate) fn configure_appkit() {
    use objc2_foundation::{NSDictionary, NSNumber, NSString, NSUserDefaults};
    let key = NSString::from_str("NSFullScreenMenuItemEverywhere");
    let value = NSNumber::new_bool(false);
    let defaults = NSDictionary::from_slices(&[&*key], &[&*value as &objc2::runtime::AnyObject]);
    unsafe { NSUserDefaults::standardUserDefaults().registerDefaults(&defaults) };
}

#[cfg(debug_assertions)]
pub(crate) mod dump;

#[cfg(test)]
mod tests;
