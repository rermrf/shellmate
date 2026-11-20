use super::*;
use crate::workspace;

#[test]
fn edit_target_prefers_inputs_and_never_dispatches_twice() {
    assert_eq!(edit_target(false, false), None);
    assert_eq!(edit_target(true, false), Some(Target::Input));
    assert_eq!(edit_target(false, true), Some(Target::Terminal));
    assert_eq!(edit_target(true, true), Some(Target::Input));
}

#[test]
#[cfg(debug_assertions)]
fn menu_tree_order_separators_and_shortcuts() {
    let mut keymap = gpui::Keymap::new(global_bindings());
    keymap.add_bindings(workspace::bindings());
    keymap.add_bindings(terminal::bindings());
    let tree = menus(&MenuState::default()).into_iter().map(|menu| {
        let entries = menu.items.into_iter().map(|item| match item {
            MenuItem::Action { name, action, checked, disabled, .. } => {
                assert!(!checked && !disabled);
                let key = dump::shortcut(&keymap, action.as_ref());
                if key == "—" { name.to_string() } else { format!("{name} {key}") }
            }
            MenuItem::Separator => "|".into(),
            MenuItem::SystemMenu(system) if system.menu_type == SystemMenuType::Services => system.name.to_string(),
            _ => panic!("unexpected submenu"),
        }).collect::<Vec<_>>().join(" / ");
        format!("{}: {entries}", menu.name)
    }).collect::<Vec<_>>();
    assert_eq!(tree, [
        "Shellmate: 关于 Shellmate / 检查更新… / | / 设置… ⌘, / | / 服务 / | / 隐藏 Shellmate ⌘H / 隐藏其他 ⌥⌘H / 全部显示 / | / 退出 Shellmate ⌘Q",
        "会话: 新建本机终端 ⌘T / 新建窗口 ⇧⌘N / 新建连接… ⌘N / 复制当前会话 ⇧⌘T / | / 分屏 ⌘D / 开始录制 / 广播输入 / | / 导入主机… / 导入配置包… / 导出配置包… / | / 关闭会话 ⌘W / 关闭窗口 ⇧⌘W",
        "编辑: 撤销 ⌘Z / 重做 ⇧⌘Z / | / 剪切 ⌘X / 拷贝 ⌘C / 粘贴 ⌘V / 全选 ⌘A / | / 查找… ⌘F / 查找下一个 ⌘G / 查找上一个 ⇧⌘G",
        "显示: 显示侧边栏 ⌃⌘S / | / 终端 ⌥⌘1 / 文件 ⌥⌘2 / 端口转发 ⌥⌘3 / 监控 ⌥⌘4 / | / 深色外观 / | / 进入全屏幕 ⌃⌘F",
        "工具: 命令面板… ⌘K / 命令历史… ⌘Y / 快捷命令 / | / AI 助手 / | / 会话记录…",
        "窗口: 最小化 ⌘M / 缩放 / | / 将会话移到新窗口 / | / 上一个会话 ⇧⌘[ / 下一个会话 ⇧⌘]",
    ]);
}

#[test]
fn menu_labels_and_checks_follow_workspace_state() {
    for panel in [Panel::Terminal, Panel::Files, Panel::Forward, Panel::Monitor] {
        let state = MenuState { sidebar_open: true, panel: Some(panel), split: true, broadcast: true,
            dark: true, recording: true, commands_open: panel == Panel::Terminal, ai_open: panel != Panel::Terminal, vault_lock: false };
        let items = menus(&state).into_iter().flat_map(|menu| menu.items).filter_map(|item| match item {
            MenuItem::Action { name, checked, .. } => Some((name.to_string(), checked)), _ => None,
        }).collect::<std::collections::HashMap<_, _>>();
        assert!(items.contains_key("隐藏侧边栏") && !items.contains_key("显示侧边栏"));
        assert!(items.contains_key("停止录制") && !items.contains_key("开始录制"));
        let panel_label = match panel { Panel::Terminal => "终端", Panel::Files => "文件", Panel::Forward => "端口转发", Panel::Monitor => "监控" };
        let drawer = if state.ai_open { "AI 助手" } else { "快捷命令" };
        for (name, checked) in items {
            assert_eq!(checked, ["分屏", "广播输入", "深色外观", panel_label, drawer].contains(&name.as_str()), "{name}");
        }
    }
}

#[test]
#[cfg(debug_assertions)]
fn keymap_has_unique_globals_and_preserves_focused_shortcuts() {
    use gpui::{KeyContext, Keymap, Keystroke};
    let mut keymap = Keymap::new(global_bindings());
    keymap.add_bindings([
        KeyBinding::new("cmd-a", input::SelectAll, Some("Input")), KeyBinding::new("cmd-c", input::Copy, Some("Input")),
        KeyBinding::new("cmd-v", input::Paste, Some("Input")), KeyBinding::new("cmd-x", input::Cut, Some("Input")),
        KeyBinding::new("cmd-z", input::Undo, Some("Input")), KeyBinding::new("cmd-shift-z", input::Redo, Some("Input")),
    ]);
    keymap.add_bindings(terminal::bindings());
    keymap.add_bindings(workspace::bindings());
    dump::audit_keymap(&keymap).unwrap();
    let contexts = [KeyContext::parse("Workspace").unwrap(), KeyContext::parse("Terminal").unwrap()];
    for binding in global_bindings().into_iter().chain(workspace::bindings()) {
        if binding.action().name().starts_with("shellmate::Edit") { continue; }
        let (matches, _) = keymap.bindings_for_input(binding.keystrokes(), &contexts);
        assert!(matches.first().unwrap().action().partial_eq(binding.action()));
    }
    assert!(keymap.bindings_for_input(&[Keystroke::parse("ctrl-r").unwrap()], &contexts).0.is_empty());
    let mut wrong_order = Keymap::new(terminal::bindings());
    wrong_order.add_bindings(global_bindings());
    assert!(wrong_order.bindings_for_input(&[Keystroke::parse("cmd-c").unwrap()], &contexts).0[0].action().partial_eq(&EditCopy));
    keymap.add_bindings([KeyBinding::new("cmd-h", ShowAll, None)]);
    assert!(dump::audit_keymap(&keymap).unwrap_err().contains("duplicate global"));
}
