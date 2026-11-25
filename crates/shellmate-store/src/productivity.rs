//! Ordered quick commands and local command history. Command text is never logged.
use crate::{HostStore, Result};
use rusqlite::params;

pub const HISTORY_ENABLED: &str = "history_enabled";
pub const HISTORY_SHORTCUT: &str = "history_shortcut";
pub const HISTORY_LIMIT: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    pub id: String,
    pub name: String,
    pub command: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnippetGroup {
    pub id: String,
    pub name: String,
    pub items: Vec<Snippet>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: i64,
    /// Stable identity, deliberately not a foreign key: deleted hosts keep their history.
    pub host: String,
    pub label: String,
    pub command: String,
    pub cwd: Option<String>,
    pub time: i64,
}

fn identity(id: &str) -> String { if id.is_empty() { uuid::Uuid::new_v4().to_string() } else { id.to_owned() } }

impl HostStore {
    pub fn history_enabled(&self) -> Result<bool> {
        Ok(self.setting(HISTORY_ENABLED)?.as_deref() != Some("0"))
    }

    pub fn history_shortcut(&self) -> Result<bool> {
        Ok(self.setting(HISTORY_SHORTCUT)?.as_deref() != Some("0"))
    }

    pub fn snippets(&self) -> Result<Vec<SnippetGroup>> {
        let groups = self
            .conn
            .prepare("SELECT id, name, stable_id FROM snippet_groups ORDER BY sort, id")?
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        groups
            .into_iter()
            .map(|(id, name, stable_id)| {
                let items = self
                    .conn
                    .prepare("SELECT name, command, stable_id FROM snippets WHERE group_id=?1 ORDER BY sort, id")?
                    .query_map([id], |r| {
                        Ok(Snippet { id: r.get(2)?,
                            name: r.get(0)?,
                            command: r.get(1)?,
                        })
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(SnippetGroup { id: stable_id, name, items })
            })
            .collect()
    }

    /// Commits a management draft atomically, including group and command ordering.
    pub fn save_snippets(&mut self, groups: &[SnippetGroup]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM snippet_groups", [])?;
        for (sort, group) in groups.iter().enumerate() {
            tx.execute(
                "INSERT INTO snippet_groups (name, sort, stable_id) VALUES (?1, ?2, ?3)",
                params![group.name, sort as i64, identity(&group.id)],
            )?;
            let id = tx.last_insert_rowid();
            for (sort, item) in group.items.iter().enumerate() {
                tx.execute(
                    "INSERT INTO snippets (group_id, name, command, sort, stable_id) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, item.name, item.command, sort as i64, identity(&item.id)],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_history(&mut self, entry: &HistoryEntry) -> Result<()> {
        if !self.history_enabled()? {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO history (host, label, command, cwd, time) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![entry.host, entry.label, entry.command, entry.cwd, entry.time],
        )?;
        tx.execute("DELETE FROM history WHERE id IN (SELECT id FROM history ORDER BY time DESC, id DESC LIMIT -1 OFFSET ?1)", [HISTORY_LIMIT as i64])?;
        tx.commit()?;
        Ok(())
    }

    /// Newest first, deduplicated within the scope before limiting. LIKE metacharacters are literal.
    pub fn history(&self, query: &str, host: Option<&str>, limit: usize) -> Result<Vec<HistoryEntry>> {
        let query = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        Ok(self
            .conn
            .prepare(
                "SELECT id, host, label, command, cwd, time FROM (
                SELECT *, ROW_NUMBER() OVER (PARTITION BY command ORDER BY time DESC, id DESC) AS rank
                FROM history WHERE (?1 IS NULL OR host=?1) AND command LIKE ?2 ESCAPE '\\'
             ) WHERE rank=1 ORDER BY time DESC, id DESC LIMIT ?3",
            )?
            .query_map(params![host, format!("%{query}%"), limit as i64], |r| {
                Ok(HistoryEntry {
                    id: r.get(0)?,
                    host: r.get(1)?,
                    label: r.get(2)?,
                    command: r.get(3)?,
                    cwd: r.get(4)?,
                    time: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn clear_history(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM history", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_core::Host;

    fn entry(command: &str, host: &str, time: i64) -> HistoryEntry {
        HistoryEntry {
            id: 0,
            command: command.into(),
            host: host.into(),
            label: "主机".into(),
            cwd: Some("/tmp".into()),
            time,
        }
    }

    #[test]
    fn migrates_v3_without_losing_hosts_or_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v3.db");
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        crate::hosts::migrate_for_tests(&mut conn, 3);
        conn.execute("INSERT INTO settings VALUES ('history_enabled', '0')", [])
            .unwrap();
        let host = Host::new("kept", "example.test");
        conn.execute("INSERT INTO hosts(id,name,addr,port,protocol,auth) VALUES(?1,?2,?3,22,'ssh','private_key')",rusqlite::params![host.id.as_str(),host.name,host.addr]).unwrap();
        drop(conn);
        let store = HostStore::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 11);
        assert!(!store.history_enabled().unwrap());
        assert!(store.history_shortcut().unwrap());
        assert!(store.snippets().unwrap().is_empty());
        store.conn.execute("INSERT INTO recordings (title, host, label, path, started_at) VALUES ('demo','local','本机','demo.cast',1)", []).unwrap();
        assert_eq!(store.hosts().unwrap()[0].id, host.id);
        drop(store);
        assert_eq!(HostStore::open(path).unwrap().hosts().unwrap().len(), 1);
    }

    #[test]
    fn snippets_crud_and_order_roundtrip() {
        let mut store = HostStore::open_in_memory().unwrap();
        let mut groups = vec![
            SnippetGroup { id: uuid::Uuid::new_v4().to_string(),
                name: "服务".into(),
                items: vec![
                    Snippet { id: uuid::Uuid::new_v4().to_string(),
                        name: "负载".into(),
                        command: "uptime".into(),
                    },
                    Snippet { id: uuid::Uuid::new_v4().to_string(),
                        name: "磁盘".into(),
                        command: "df -h".into(),
                    },
                ],
            },
            SnippetGroup { id: uuid::Uuid::new_v4().to_string(),
                name: "空组".into(),
                items: vec![],
            },
        ];
        store.save_snippets(&groups).unwrap();
        assert_eq!(store.snippets().unwrap(), groups);
        groups.swap(0, 1);
        groups[1].items[0].command = "uptime -p".into();
        groups[1].items.swap(0, 1);
        store.save_snippets(&groups).unwrap();
        assert_eq!(store.snippets().unwrap(), groups);
        store.save_snippets(&[]).unwrap();
        assert!(store.snippets().unwrap().is_empty());
    }

    #[test]
    fn history_scope_dedup_deletion_settings_and_literal_search() {
        let mut store = HostStore::open_in_memory().unwrap();
        let host = Host::new("deleted", "example.test");
        store.save_host(&host).unwrap();
        store.add_history(&entry("uptime", host.id.as_str(), 1)).unwrap();
        store.add_history(&entry("uptime", "local", 2)).unwrap();
        store.add_history(&entry("echo 100%_", "local", 3)).unwrap();
        store.delete_host(&host.id).unwrap();
        assert_eq!(store.history("", None, 8).unwrap().len(), 2);
        let rows = store.history("", Some(host.id.as_str()), 8).unwrap();
        assert_eq!(rows[0].host, host.id.as_str());
        assert_eq!(rows[0].label, "主机");
        assert_eq!(rows[0].cwd.as_deref(), Some("/tmp"));
        assert_eq!(store.history("%_", None, 8).unwrap().len(), 1);
        store.set_setting(HISTORY_ENABLED, "0").unwrap();
        store.add_history(&entry("ignored", "local", 4)).unwrap();
        assert_eq!(store.history("", None, 8).unwrap().len(), 2);
        store.clear_history().unwrap();
        assert!(store.history("", None, 8).unwrap().is_empty());
    }

    #[test]
    fn history_prunes_oldest_and_breaks_ties_by_id() {
        let mut store = HostStore::open_in_memory().unwrap();
        store.conn.execute_batch("WITH RECURSIVE seq(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM seq WHERE n<20000) INSERT INTO history(host,label,command,time) SELECT 'local','本机','echo '||n,n FROM seq").unwrap();
        store.add_history(&entry("newest", "local", 20000)).unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 20000);
        assert!(
            store
                .history("echo 1", None, 20000)
                .unwrap()
                .iter()
                .all(|r| r.command != "echo 1")
        );
        assert_eq!(store.history("", None, 1).unwrap()[0].command, "newest");
    }
}
