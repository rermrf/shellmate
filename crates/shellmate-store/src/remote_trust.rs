//! Trust is granted to a host/path location, never a content hash.
use crate::{HostStore, Result};
use rusqlite::params;
fn key(host: &str, path: &str) -> String {
    format!("ai_remote_doc_trust:{host}:{path}")
}
impl HostStore {
    pub fn remote_doc_trust(&self, host: &str, path: &str) -> Result<Option<bool>> {
        Ok(match self.setting(&key(host, path))?.as_deref() {
            Some("trust") => Some(true),
            Some("ignore") => Some(false),
            _ => None,
        })
    }
    pub fn save_remote_doc_trust(
        &mut self,
        host: &str,
        decisions: &[(String, bool)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (path, trust) in decisions {
            tx.execute("INSERT INTO settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key(host, path), if *trust { "trust" } else { "ignore" }])?;
        }
        tx.commit()?;
        Ok(())
    }
}

impl HostStore {
    pub fn remote_docs(&self, host: &str) -> Result<Vec<(String, bool)>> {
        let prefix = format!("ai_remote_doc_trust:{host}:");
        let mut statement = self
            .conn
            .prepare("SELECT key,value FROM settings WHERE substr(key,1,?1)=?2 ORDER BY key")?;
        let rows = statement
            .query_map(params![prefix.chars().count() as i64, prefix], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(key, value)| match value.as_str() {
                "trust" => Some((key[prefix.len()..].into(), true)),
                "ignore" => Some((key[prefix.len()..].into(), false)),
                _ => None,
            })
            .collect())
    }
    pub fn remove_remote_doc_trust(&mut self, host: &str, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM settings WHERE key=?1",
            params![key(host, path)],
        )?;
        Ok(())
    }
    pub fn clear_remote_doc_trust(&mut self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM settings WHERE substr(key,1,20)='ai_remote_doc_trust:'",
            [],
        )?;
        Ok(())
    }
    pub fn set_auto_memory(&mut self, enabled: bool) -> Result<()> {
        self.set_setting("ai_auto_memory", if enabled { "true" } else { "false" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trust_is_per_host_path_and_invalid_values_ask_again() {
        let mut db = HostStore::open_in_memory().unwrap();
        assert_eq!(
            db.remote_doc_trust("local", "/srv/AGENTS.md").unwrap(),
            None
        );
        db.save_remote_doc_trust(
            "local",
            &[
                ("/srv/AGENTS.md".into(), true),
                ("/srv/CLAUDE.md".into(), false),
            ],
        )
        .unwrap();
        assert_eq!(
            db.remote_doc_trust("local", "/srv/AGENTS.md").unwrap(),
            Some(true)
        );
        assert_eq!(
            db.remote_doc_trust("local", "/srv/CLAUDE.md").unwrap(),
            Some(false)
        );
        assert_eq!(
            db.remote_doc_trust("other", "/srv/AGENTS.md").unwrap(),
            None
        );
        assert_eq!(
            db.remote_doc_trust("local", "/else/AGENTS.md").unwrap(),
            None
        );
        db.set_setting("ai_remote_doc_trust:local:/srv/AGENTS.md", "bad")
            .unwrap();
        assert_eq!(
            db.remote_doc_trust("local", "/srv/AGENTS.md").unwrap(),
            None
        );
    }
}
