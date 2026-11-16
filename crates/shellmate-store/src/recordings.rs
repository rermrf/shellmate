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
        Ok(p)
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
}
impl Preference {
    pub const ALL: [Self; 9] = [
        Self::CopySelection,
        Self::RightPaste,
        Self::BracketedPaste,
        Self::History,
        Self::HistoryShortcut,
        Self::SessionLog,
        Self::AutoRecord,
        Self::LoginShell,
        Self::FollowDirectory,
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
            Self::FollowDirectory => "通过 OSC 7 / shell 集成上报当前目录",
            _ => "",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preferences {
    values: [bool; 9],
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            values: [true, true, true, true, true, false, false, true, true],
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
    fn preferences_defaults_and_each_toggle_roundtrip() {
        let mut s = HostStore::open_in_memory().unwrap();
        let p = s.preferences().unwrap();
        for key in Preference::ALL {
            assert_eq!(
                p.get(key),
                !matches!(key, Preference::SessionLog | Preference::AutoRecord)
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
        assert_eq!(s.schema_version().unwrap(), 7);
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
