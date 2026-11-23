//! Recording catalog and settings, stored in the host-library database.
use crate::{HostStore, Result};
use rusqlite::params;
#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    pub id: i64,
    pub title: String,
    pub host: String,
    pub label: String,
    pub path: String,
    pub started_at: i64,
    pub duration: f64,
    pub size: u64,
    pub cols: u16,
    pub rows: u16,
}
impl HostStore {
    pub fn recordings(&self) -> Result<Vec<Recording>> {
        Ok(self.conn.prepare("SELECT id,title,host,label,path,started_at,duration,size,cols,rows FROM recordings ORDER BY started_at DESC,id DESC")?.query_map([],|r|Ok(Recording{id:r.get(0)?,title:r.get(1)?,host:r.get(2)?,label:r.get(3)?,path:r.get(4)?,started_at:r.get(5)?,duration:r.get(6)?,size:r.get::<_,i64>(7)?.max(0) as u64,cols:r.get(8)?,rows:r.get(9)?}))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn add_recording(&mut self, r: &Recording) -> Result<i64> {
        self.conn.execute("INSERT INTO recordings(title,host,label,path,started_at,duration,size,cols,rows) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![r.title,r.host,r.label,r.path,r.started_at,r.duration,r.size as i64,r.cols,r.rows])?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn finish_recording(&mut self, id: i64, duration: f64, size: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE recordings SET duration=?2,size=?3 WHERE id=?1",
            params![id, duration, size as i64],
        )?;
        Ok(())
    }
    pub fn rename_recording(&mut self, id: i64, title: &str) -> Result<()> {
        self.conn
            .execute("UPDATE recordings SET title=?2 WHERE id=?1", params![id, title])?;
        Ok(())
    }
    pub fn delete_recording(&mut self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM recordings WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn preferences(&self) -> Result<Preferences> {
        let mut p = Preferences::default();
        for key in Preference::ALL {
            if let Some(value) = self.setting(key.key())? {
                p.set(key, value != "0");
            }
        }
        p.close_window_with_sessions = CloseWindowWithSessions::parse(self.setting(CloseWindowWithSessions::KEY)?.as_deref());
        p.terminal_font_size = TerminalFontSize::parse(self.setting(TerminalFontSize::KEY)?.as_deref());
        Ok(p)
    }
    pub fn set_close_window_with_sessions(&mut self, value: CloseWindowWithSessions) -> Result<()> {
        self.set_setting(CloseWindowWithSessions::KEY, value.as_str())
    }
    /// Saves the default terminal font size (clamped to the supported range) and returns what was saved.
    pub fn set_terminal_font_size(&mut self, points: u8) -> Result<u8> {
        let points = TerminalFontSize::clamp(points);
        self.set_setting(TerminalFontSize::KEY, &points.to_string())?;
        Ok(points)
    }
    pub fn set_preference(&mut self, key: Preference, value: bool) -> Result<()> {
        self.set_setting(key.key(), if value { "1" } else { "0" })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preference {
    CopySelection,
    RightPaste,
    BracketedPaste,
    History,
    HistoryShortcut,
    SessionLog,
    AutoRecord,
    LoginShell,
    FollowDirectory,
    PromptHighlight,
    ReplayTimestamps,
    PreloadPanels,
    QuitOnWindowClose,
    ConfirmQuit,
    ShowStatusIcon,
}
impl Preference {
    pub const ALL: [Self; 15] = [
        Self::CopySelection,
        Self::RightPaste,
        Self::BracketedPaste,
        Self::History,
        Self::HistoryShortcut,
        Self::SessionLog,
        Self::AutoRecord,
        Self::LoginShell,
        Self::FollowDirectory,
        Self::PromptHighlight,
        Self::ReplayTimestamps,
        Self::PreloadPanels,
        Self::QuitOnWindowClose,
        Self::ConfirmQuit,
        Self::ShowStatusIcon,
    ];
    pub fn key(self) -> &'static str {
        match self {
            Self::CopySelection => "copy_selection",
            Self::RightPaste => "right_click_paste",
            Self::BracketedPaste => "bracketed_paste",
            Self::History => crate::HISTORY_ENABLED,
            Self::HistoryShortcut => crate::HISTORY_SHORTCUT,
            Self::SessionLog => "session_log",
            Self::AutoRecord => "auto_record",
            Self::LoginShell => "login_shell",
            Self::FollowDirectory => "follow_directory",
            Self::PromptHighlight => "prompt_highlight",
            Self::ReplayTimestamps => "replay_timestamps",
            Self::PreloadPanels => "preload_panels",
            Self::QuitOnWindowClose => "quit_on_window_close",
            Self::ConfirmQuit => "confirm_quit",
            Self::ShowStatusIcon => "show_status_icon",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::CopySelection => "选中即复制",
            Self::RightPaste => "右键粘贴",
            Self::BracketedPaste => "括号粘贴模式",
            Self::History => "记录命令历史",
            Self::HistoryShortcut => "⌃R 打开 Shellmate 历史",
            Self::SessionLog => "保存会话日志",
            Self::AutoRecord => "自动录制所有会话",
            Self::LoginShell => "作为登录 shell 启动（-l）",
            Self::FollowDirectory => "文件面板跟随终端目录",
            Self::PromptHighlight => "提示符高亮",
            Self::ReplayTimestamps => "回放时间",
            Self::PreloadPanels => "连接后预加载文件与监控",
            Self::QuitOnWindowClose => "关闭窗口时",
            Self::ConfirmQuit => "退出前确认",
            Self::ShowStatusIcon => "在菜单栏显示图标",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::BracketedPaste => "多行粘贴不会被逐行执行",
            Self::History => "历史保存在本机，可随时清空",
            Self::HistoryShortcut => "关闭后 ⌃R 交给 shell 的反向搜索",
            Self::SessionLog => "纯文本，按主机与日期归档；包含屏幕上的输出，可能含敏感信息",
            Self::AutoRecord => "对新会话生效",
            Self::LoginShell => "加载 ~/.zprofile，PATH 与系统终端一致 · 对新会话生效",
            Self::ShowStatusIcon => "关闭窗口后，Shellmate 只保留菜单栏图标；关掉此项则保留 Dock 图标。",
            Self::FollowDirectory => "通过 OSC 7 / shell 集成上报当前目录",
            Self::PreloadPanels => "SSH 连接后在后台打开文件列表、探测系统信息，打开文件、监控时无需等待",
            _ => "",
        }
    }
}
/// Policy for closing a nonfinal workspace that still contains sessions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CloseWindowWithSessions {
    #[default]
    Ask,
    Move,
    Disconnect,
}
impl CloseWindowWithSessions {
    pub const KEY: &'static str = "close_window_with_sessions";
    pub const ALL: [Self; 3] = [Self::Ask, Self::Move, Self::Disconnect];
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("move") => Self::Move,
            Some("disconnect") => Self::Disconnect,
            _ => Self::Ask,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self { Self::Ask => "ask", Self::Move => "move", Self::Disconnect => "disconnect" }
    }
    pub fn label(self) -> &'static str {
        match self { Self::Ask => "按需询问", Self::Move => "移到其他窗口", Self::Disconnect => "断开并关闭" }
    }
}

/// The default font size of terminals, in points. A window can zoom away from it temporarily.
pub struct TerminalFontSize;
impl TerminalFontSize {
    pub const KEY: &'static str = "terminal.font_size";
    pub const MIN: u8 = 9;
    pub const MAX: u8 = 28;
    pub const DEFAULT: u8 = 13;
    pub fn clamp(points: u8) -> u8 {
        points.clamp(Self::MIN, Self::MAX)
    }
    /// A stored value; anything that is not a whole number of points falls back to the default.
    pub fn parse(value: Option<&str>) -> u8 {
        value
            .and_then(|text| text.trim().parse::<u8>().ok())
            .map_or(Self::DEFAULT, Self::clamp)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preferences {
    pub close_window_with_sessions: CloseWindowWithSessions,
    /// Default terminal font size in points ([`TerminalFontSize`]).
    pub terminal_font_size: u8,
    values: [bool; Preference::ALL.len()],
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            close_window_with_sessions: CloseWindowWithSessions::Ask,
            terminal_font_size: TerminalFontSize::DEFAULT,
            values: [true, true, true, true, true, false, false, true, true, true, true, true, false, true, true],
        }
    }
}
impl Preferences {
    pub fn get(&self, key: Preference) -> bool {
        self.values[key as usize]
    }
    pub fn set(&mut self, key: Preference, value: bool) {
        self.values[key as usize] = value;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn close_window_policy_defaults_roundtrips_and_rejects_unknown_values() {
        let mut store = HostStore::open_in_memory().unwrap();
        assert_eq!(store.preferences().unwrap().close_window_with_sessions, CloseWindowWithSessions::Ask);
        for value in CloseWindowWithSessions::ALL {
            store.set_close_window_with_sessions(value).unwrap();
            assert_eq!(store.setting(CloseWindowWithSessions::KEY).unwrap().as_deref(), Some(value.as_str()));
            assert_eq!(store.preferences().unwrap().close_window_with_sessions, value);
        }
        for invalid in ["", "MOVE", "0", "unknown"] {
            store.set_setting(CloseWindowWithSessions::KEY, invalid).unwrap();
            assert_eq!(store.preferences().unwrap().close_window_with_sessions, CloseWindowWithSessions::Ask);
        }
    }

    #[test]
    fn terminal_font_size_defaults_roundtrips_and_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("font.db");
        let mut store = HostStore::open(&path).unwrap();
        assert_eq!(store.preferences().unwrap().terminal_font_size, 13, "nothing saved yet");
        assert_eq!(Preferences::default().terminal_font_size, TerminalFontSize::DEFAULT);
        for (asked, saved) in [(16, 16), (9, 9), (28, 28), (0, 9), (8, 9), (29, 28), (255, 28)] {
            assert_eq!(store.set_terminal_font_size(asked).unwrap(), saved);
            drop(store);
            store = HostStore::open(&path).unwrap();
            assert_eq!(store.preferences().unwrap().terminal_font_size, saved, "asked for {asked}");
        }
        assert_eq!(store.setting("terminal.font_size").unwrap().as_deref(), Some("28"));
    }

    #[test]
    fn stored_font_sizes_that_make_no_sense_fall_back_or_clamp() {
        let mut store = HostStore::open_in_memory().unwrap();
        for (stored, expected) in [("14", 14), (" 15 ", 15), ("3", 9), ("99", 28), ("-4", 13), ("13.5", 13), ("big", 13), ("", 13), ("300", 13)] {
            store.set_setting(TerminalFontSize::KEY, stored).unwrap();
            assert_eq!(store.preferences().unwrap().terminal_font_size, expected, "stored {stored:?}");
        }
        assert_eq!(TerminalFontSize::parse(None), 13);
    }

    #[test]
    fn window_preferences_persist_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.db");
        let mut store = HostStore::open(&path).unwrap();
        for (quit, confirm) in [(false, true), (true, false), (false, true)] {
            let prefs = store.preferences().unwrap();
            assert_eq!(prefs.get(Preference::QuitOnWindowClose), quit);
            assert_eq!(prefs.get(Preference::ConfirmQuit), confirm);
            assert_eq!(prefs.get(Preference::ShowStatusIcon), confirm);
            store.set_preference(Preference::ShowStatusIcon, !confirm).unwrap();
            store.set_preference(Preference::QuitOnWindowClose, !quit).unwrap();
            store.set_preference(Preference::ConfirmQuit, !confirm).unwrap();
            drop(store);
            store = HostStore::open(&path).unwrap();
        }
    }

    #[test]
    fn replay_timestamps_default_on_and_persist_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.db");
        let mut store = HostStore::open(&path).unwrap();
        assert!(store.preferences().unwrap().get(Preference::ReplayTimestamps));
        for on in [false, true] {
            store.set_preference(Preference::ReplayTimestamps, on).unwrap();
            drop(store);
            store = HostStore::open(&path).unwrap();
            assert_eq!(store.preferences().unwrap().get(Preference::ReplayTimestamps), on);
        }
    }
    #[test]
    fn prompt_highlight_defaults_on_and_persists_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.db");
        let mut store = HostStore::open(&path).unwrap();
        assert!(store.preferences().unwrap().get(Preference::PromptHighlight));
        assert_eq!(Preference::PromptHighlight.label(), "提示符高亮");
        assert_eq!(Preference::PromptHighlight.description(), "");
        store.set_preference(Preference::PromptHighlight, false).unwrap();
        drop(store);
        let mut store = HostStore::open(&path).unwrap();
        assert!(!store.preferences().unwrap().get(Preference::PromptHighlight));
        store.set_preference(Preference::PromptHighlight, true).unwrap();
        drop(store);
        assert!(HostStore::open(&path).unwrap().preferences().unwrap().get(Preference::PromptHighlight));
    }
    #[test]
    fn preferences_defaults_and_each_toggle_roundtrip() {
        let mut s = HostStore::open_in_memory().unwrap();
        let p = s.preferences().unwrap();
        assert!(p.get(Preference::PreloadPanels));
        for key in Preference::ALL {
            assert_eq!(
                p.get(key),
                !matches!(key, Preference::SessionLog | Preference::AutoRecord | Preference::QuitOnWindowClose)
            );
            s.set_preference(key, !p.get(key)).unwrap();
            assert_eq!(s.preferences().unwrap().get(key), !p.get(key));
        }
        s.set_preference(Preference::History, false).unwrap();
        assert!(!s.history_enabled().unwrap());
        s.set_preference(Preference::HistoryShortcut, false).unwrap();
        assert!(!s.history_shortcut().unwrap());
    }
    #[test]
    fn auto_record_default_and_toggle_only_affect_new_session_policy() {
        let mut store = HostStore::open_in_memory().unwrap();
        let initial = store.preferences().unwrap();
        assert!(!initial.get(Preference::AutoRecord));
        store.set_preference(Preference::AutoRecord, true).unwrap();
        let new_session = store.preferences().unwrap();
        assert!(new_session.get(Preference::AutoRecord));
        assert!(!initial.get(Preference::AutoRecord));
        store.set_preference(Preference::AutoRecord, false).unwrap();
        assert!(!store.preferences().unwrap().get(Preference::AutoRecord));
    }
    #[test]
    fn v4_migrates_catalog_and_preserves_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v4.db");
        let mut c = rusqlite::Connection::open(&path).unwrap();
        crate::hosts::migrate_for_tests(&mut c, 4);
        c.execute(
            "INSERT INTO recordings(title,host,label,path,started_at) VALUES('old','h','host','x.cast',1)",
            [],
        )
        .unwrap();
        drop(c);
        let mut s = HostStore::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 9);
        let r = &s.recordings().unwrap()[0];
        assert_eq!((r.cols, r.rows), (80, 24));
        let mut new = r.clone();
        new.cols = 100;
        new.rows = 30;
        let id = s.add_recording(&new).unwrap();
        s.finish_recording(id, 12.5, 256).unwrap();
        s.rename_recording(id, "renamed").unwrap();
        let r = &s.recordings().unwrap()[0];
        assert_eq!(
            (r.title.as_str(), r.duration, r.size as i64, r.cols, r.rows),
            ("renamed", 12.5, 256, 100, 30)
        );
        s.delete_recording(id).unwrap();
        assert_eq!(s.recordings().unwrap().len(), 1);
    }
}
