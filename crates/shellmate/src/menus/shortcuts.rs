//! What the keyboard shortcut sheet (帮助 › 键盘快捷键) lists, and where each key comes from.
//!
//! A row names an action, and its keys are read from the keymap: whatever is bound is shown, so
//! the sheet cannot drift from the real bindings. The few keys that no binding covers (the
//! ⌘1–⌘9 of `Workspace::key_down`, the ⌘← of the terminal) are typed here, and every one of
//! them is listed in [`MANUAL`] with the code that handles it. The tests below keep the three
//! in step: the sheet, the keymap and the menus.
use gpui::{Action, Keymap};
use gpui::component::kbd::Kbd;
use shellmate_term::view as terminal;

use super::*;

pub(crate) enum Keys {
    /// Every key the keymap binds to the action.
    Bound(&'static dyn Action),
    /// Keys no binding covers. `shown` is the text of the row; `keys` are the keystrokes behind
    /// it, each one in [`MANUAL`].
    Manual {
        shown: &'static [&'static str],
        #[cfg_attr(not(test), allow(dead_code))] // the tests check these against `MANUAL`
        keys: &'static [&'static str],
    },
}

pub(crate) struct Row {
    pub label: &'static str,
    pub keys: Keys,
}

pub(crate) struct Section {
    pub title: &'static str,
    pub rows: Vec<Row>,
}

/// Which code reads a keystroke that has no binding.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Handler {
    /// `Workspace::key_down`: ⌘1…⌘9 select a tab.
    TabNumber,
    /// The terminal turns the key into input for the program (`shellmate_term::sends_input`).
    TerminalInput,
}

#[cfg(test)]
impl Handler {
    pub(crate) fn handles(self, keystroke: &gpui::Keystroke) -> bool {
        match self {
            Self::TabNumber => crate::workspace::tab_number(keystroke).is_some(),
            Self::TerminalInput => shellmate_term::sends_input(keystroke),
        }
    }
}

/// The keystrokes that work without a binding, and who handles them.
#[cfg(test)]
pub(crate) const MANUAL: &[(&str, Handler)] = &[
    ("cmd-1", Handler::TabNumber), ("cmd-2", Handler::TabNumber), ("cmd-3", Handler::TabNumber),
    ("cmd-4", Handler::TabNumber), ("cmd-5", Handler::TabNumber), ("cmd-6", Handler::TabNumber),
    ("cmd-7", Handler::TabNumber), ("cmd-8", Handler::TabNumber), ("cmd-9", Handler::TabNumber),
    ("cmd-left", Handler::TerminalInput), ("cmd-right", Handler::TerminalInput),
    ("cmd-backspace", Handler::TerminalInput),
    ("alt-left", Handler::TerminalInput), ("alt-right", Handler::TerminalInput),
];

fn bound(label: &'static str, action: &'static dyn Action) -> Row {
    Row { label, keys: Keys::Bound(action) }
}

fn manual(label: &'static str, shown: &'static [&'static str], keys: &'static [&'static str]) -> Row {
    Row { label, keys: Keys::Manual { shown, keys } }
}

/// The sheet, in reading order. Labels follow the menus.
pub(crate) fn sections() -> Vec<Section> {
    vec![
        Section { title: "会话与标签", rows: vec![
            bound("新建本机终端", &NewLocalTerminal),
            bound("新建连接", &NewConnection),
            bound("复制当前会话", &DuplicateSession),
            bound("分屏", &ToggleSplit),
            bound("关闭会话", &CloseTab),
            bound("下一个会话", &NextTab),
            bound("上一个会话", &PreviousTab),
            manual("切换到第 1–8 个会话", &["⌘1–⌘8"],
                &["cmd-1", "cmd-2", "cmd-3", "cmd-4", "cmd-5", "cmd-6", "cmd-7", "cmd-8"]),
            manual("切换到最后一个会话", &["⌘9"], &["cmd-9"]),
        ] },
        Section { title: "窗口", rows: vec![
            bound("新建窗口", &NewWindow),
            bound("关闭窗口", &CloseWindow),
            bound("最小化", &Minimize),
            bound("进入全屏幕", &Fullscreen),
            bound("设置", &Settings),
            bound("隐藏 Shellmate", &HideApp),
            bound("隐藏其他", &HideOthers),
            bound("退出 Shellmate", &Quit),
        ] },
        Section { title: "显示", rows: vec![
            bound("显示或隐藏侧边栏", &ToggleSidebar),
            bound("终端", &ShowTerminal),
            bound("文件", &ShowFiles),
            bound("端口转发", &ShowForward),
            bound("监控", &ShowMonitor),
            bound("放大", &IncreaseFontSize),
            bound("缩小", &DecreaseFontSize),
            bound("实际大小", &ResetFontSize),
        ] },
        Section { title: "编辑与终端", rows: vec![
            bound("撤销", &EditUndo),
            bound("重做", &EditRedo),
            bound("剪切", &EditCut),
            bound("拷贝", &EditCopy),
            bound("粘贴", &EditPaste),
            bound("全选", &EditSelectAll),
            bound("查找", &terminal::ToggleSearch),
            bound("查找下一个", &terminal::SearchUp),
            bound("查找上一个", &terminal::SearchDown),
            bound("关闭查找", &terminal::CloseSearch),
            bound("向上翻页", &terminal::ScrollPageUp),
            bound("向下翻页", &terminal::ScrollPageDown),
            bound("滚动到顶部", &terminal::ScrollToTop),
            bound("滚动到底部", &terminal::ScrollToBottom),
            manual("光标移到行首、行尾", &["⌘←", "⌘→"], &["cmd-left", "cmd-right"]),
            manual("删除整行", &["⌘⌫"], &["cmd-backspace"]),
            manual("按词移动光标", &["⌥←", "⌥→"], &["alt-left", "alt-right"]),
        ] },
        Section { title: "工具", rows: vec![
            bound("命令面板", &OpenPalette),
            bound("命令历史", &History),
            bound("键盘快捷键", &ShowShortcuts),
        ] },
    ]
}

/// Keys the keymap binds that the sheet leaves out on purpose, each shown under another row:
/// the terminal's own ⌘C ⌘V ⌘A (the 编辑 menu's rows carry the same keys) and Tab, which belongs to the shell.
#[cfg(test)]
pub(crate) fn unlisted() -> [&'static dyn Action; 5] {
    [&terminal::Copy, &terminal::Paste, &terminal::SelectAll, &terminal::SendTab, &terminal::SendBackTab]
}

/// The keys of a row as they are written on the sheet, for a keymap (each key once, in binding order).
pub(crate) fn chips(keys: &Keys, keymap: &Keymap) -> Vec<String> {
    match keys {
        Keys::Bound(action) => {
            let mut shown: Vec<String> = Vec::new();
            for binding in keymap.bindings_for_action(*action) {
                // A chord has no single chip; none of the app's bindings is one.
                if let [key] = binding.keystrokes() {
                    let text = Kbd::format(gpui::AsKeystroke::as_keystroke(key));
                    if !shown.contains(&text) { shown.push(text); }
                }
            }
            shown
        }
        Keys::Manual { shown, .. } => shown.iter().map(|text| (*text).to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AsKeystroke as _, Keystroke};

    /// The bindings the app registers for these three scopes (see `init`, `workspace::init`, `shellmate_term::init`).
    fn keymap() -> Keymap {
        let mut keymap = Keymap::new(global_bindings());
        keymap.add_bindings(crate::workspace::bindings());
        keymap.add_bindings(terminal::bindings());
        keymap
    }

    fn listed_actions() -> Vec<&'static dyn Action> {
        sections().iter().flat_map(|s| &s.rows).filter_map(|row| match row.keys {
            Keys::Bound(action) => Some(action),
            Keys::Manual { .. } => None,
        }).collect()
    }

    fn is_bound(keymap: &Keymap, keystroke: &Keystroke) -> bool {
        keymap.bindings().any(|binding| matches!(binding.keystrokes(), [only] if only.as_keystroke() == keystroke))
    }

    fn lists(actions: &[&dyn Action], action: &dyn Action) -> bool {
        actions.iter().any(|listed| listed.partial_eq(action))
    }

    #[test]
    fn every_key_on_the_sheet_is_bound_or_handled_by_hand() {
        let keymap = keymap();
        for row in sections().iter().flat_map(|s| &s.rows) {
            match &row.keys {
                Keys::Bound(action) => {
                    let keys = chips(&row.keys, &keymap);
                    assert!(!keys.is_empty(), "{} ({}) has no binding to show", row.label, action.name());
                }
                Keys::Manual { shown, keys } => {
                    assert!(!shown.is_empty() && !keys.is_empty(), "{}", row.label);
                    for key in *keys {
                        assert!(MANUAL.iter().any(|(listed, _)| listed == key), "{key} is not in the manual list");
                    }
                }
            }
        }
    }

    #[test]
    fn manual_keys_are_really_handled_and_really_not_bound() {
        let keymap = keymap();
        let mut used = std::collections::HashSet::new();
        for row in sections().iter().flat_map(|s| &s.rows) {
            if let Keys::Manual { keys, .. } = &row.keys { used.extend(keys.iter().copied()); }
        }
        for &(key, handler) in MANUAL {
            let keystroke = Keystroke::parse(key).unwrap();
            assert!(handler.handles(&keystroke), "{key}: {handler:?} does not handle it");
            assert!(!is_bound(&keymap, &keystroke), "{key} is bound now: list its action instead of typing the key");
            assert!(used.contains(key), "{key} is in the manual list but on no row of the sheet");
        }
    }

    #[test]
    fn the_manual_handlers_reject_what_they_do_not_handle() {
        for key in ["cmd-0", "cmd-shift-1", "alt-cmd-1", "ctrl-1", "1", "cmd-a", "cmd-=", "cmd-/"] {
            assert!(!Handler::TabNumber.handles(&Keystroke::parse(key).unwrap()), "{key}");
        }
        for key in ["cmd-=", "cmd-0", "cmd-/", "cmd-k", "cmd-t"] {
            assert!(!Handler::TerminalInput.handles(&Keystroke::parse(key).unwrap()), "{key}");
        }
    }

    #[test]
    fn every_menu_shortcut_is_on_the_sheet() {
        let keymap = keymap();
        let listed = listed_actions();
        let mut checked = 0;
        for item in menus(&MenuState::default()).into_iter().flat_map(|menu| menu.items) {
            let MenuItem::Action { name, action, .. } = item else { continue };
            if keymap.bindings_for_action(action.as_ref()).next().is_some() {
                assert!(lists(&listed, action.as_ref()), "the menu item {name} has a shortcut but is not on the sheet");
                checked += 1;
            }
        }
        assert!(checked >= 30, "only {checked} menu shortcuts found: is the keymap incomplete?");
    }

    #[test]
    fn every_binding_is_on_the_sheet_or_deliberately_left_out() {
        let keymap = keymap();
        let listed = listed_actions();
        let left_out = unlisted();
        for binding in keymap.bindings() {
            let action = binding.action();
            assert!(lists(&listed, action) || lists(&left_out, action),
                "{} is bound ({:?}) but neither on the sheet nor in `unlisted`", action.name(), binding.keystrokes());
        }
        // And nothing is both.
        for action in &left_out {
            assert!(!lists(&listed, *action), "{}", action.name());
        }
    }

    #[test]
    fn rows_show_the_keys_the_menus_show() {
        let keymap = keymap();
        let keys = |action: &'static dyn Action| chips(&Keys::Bound(action), &keymap);
        assert_eq!(keys(&NewLocalTerminal), ["⌘T"]);
        assert_eq!(keys(&NextTab), ["⇧⌘]", "⌃Tab"]);
        assert_eq!(keys(&IncreaseFontSize), ["⌘=", "⌘+"]);
        assert_eq!(keys(&DecreaseFontSize), ["⌘-"]);
        assert_eq!(keys(&ResetFontSize), ["⌘0"]);
        assert_eq!(keys(&ShowShortcuts), ["⌘/"]);
        assert_eq!(keys(&terminal::ToggleSearch), ["⌘F"]);
        assert_eq!(keys(&EditCopy), ["⌘C"]);
        let sheet = sections();
        let first = |title: &str, label: &str| sheet.iter().find(|s| s.title == title).and_then(|s| s.rows.iter().find(|r| r.label == label))
            .map(|row| chips(&row.keys, &keymap));
        assert_eq!(first("会话与标签", "切换到第 1–8 个会话").unwrap(), ["⌘1–⌘8"]);
        assert_eq!(first("会话与标签", "切换到最后一个会话").unwrap(), ["⌘9"]);
        assert_eq!(first("编辑与终端", "按词移动光标").unwrap(), ["⌥←", "⌥→"]);
    }

    #[test]
    fn the_sheet_has_the_five_groups_without_empty_or_repeated_rows() {
        let sheet = sections();
        let titles: Vec<_> = sheet.iter().map(|s| s.title).collect();
        assert_eq!(titles, ["会话与标签", "窗口", "显示", "编辑与终端", "工具"]);
        let mut labels = std::collections::HashSet::new();
        for section in &sheet {
            assert!(!section.rows.is_empty());
            for row in &section.rows {
                assert!(labels.insert(row.label), "{} twice", row.label);
            }
        }
        let mut seen: Vec<&dyn Action> = Vec::new();
        for action in listed_actions() {
            assert!(!lists(&seen, action), "{} is on two rows", action.name());
            seen.push(action);
        }
    }
}
