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
