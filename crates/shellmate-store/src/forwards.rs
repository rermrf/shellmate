//! The port forwarding rules of the saved hosts.

use rusqlite::{OptionalExtension, Row, params};
use shellmate_core::{HostId, ImportPlan};
use shellmate_forward::{Endpoint, ForwardKind, ForwardRule, RuleId};

use crate::error::{Result, StoreError};
use crate::hosts::{HostStore, write_hosts};

const COLUMNS: &str = "id, kind, name, listen_host, listen_port, target_host, target_port, enabled";

/// One row of `forward_rules`, before it is understood.
struct RawRule {
    id: String,
    kind: String,
    name: String,
    listen_host: String,
    listen_port: i64,
    target_host: Option<String>,
    target_port: Option<i64>,
    enabled: bool,
}

fn raw_rule(row: &Row<'_>) -> rusqlite::Result<RawRule> {
    Ok(RawRule {
        id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        listen_host: row.get(3)?,
        listen_port: row.get(4)?,
        target_host: row.get(5)?,
        target_port: row.get(6)?,
        enabled: row.get(7)?,
    })
}

fn port(value: i64) -> Result<u16> {
    // Port 0 is a real value for a remote forward (the server picks one).
    u16::try_from(value).map_err(|_| StoreError::Corrupt(format!("转发规则的端口 {value} 超出范围")))
}

impl RawRule {
    fn into_rule(self) -> Result<ForwardRule> {
        let kind = ForwardKind::from_letter(&self.kind)
            .ok_or_else(|| StoreError::Corrupt(format!("未知的转发类型 {:?}", self.kind)))?;
        let target = match (self.target_host, self.target_port) {
            (Some(host), Some(target_port)) => Some(Endpoint::new(host, port(target_port)?)),
            (None, None) => None,
            _ => return Err(StoreError::Corrupt(format!("转发规则「{}」的目标不完整", self.name))),
        };
        if kind != ForwardKind::Dynamic && target.is_none() {
            return Err(StoreError::Corrupt(format!("转发规则「{}」缺少目标地址", self.name)));
        }
        Ok(ForwardRule {
            id: RuleId::from_raw(self.id),
            name: self.name,
            kind,
            listen: Endpoint::new(self.listen_host, port(self.listen_port)?),
            target,
            enabled: self.enabled,
        })
    }
}

/// Writes `rule` for `host`: a new rule goes last, a known one is updated in place (its host and
/// position stay).
fn write_rule(tx: &rusqlite::Transaction<'_>, host: &HostId, rule: &ForwardRule) -> Result<()> {
    let exists: bool =
        tx.query_row("SELECT EXISTS(SELECT 1 FROM hosts WHERE id = ?1)", params![host.as_str()], |r| r.get(0))?;
    if !exists {
        return Err(StoreError::HostNotFound(host.clone()));
    }
    let next_sort: i64 = tx.query_row(
        "SELECT COALESCE(MAX(sort), -1) + 1 FROM forward_rules WHERE host_id = ?1",
        params![host.as_str()],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO forward_rules (id, host_id, kind, name, listen_host, listen_port, target_host, target_port, enabled, sort)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
            kind = excluded.kind, name = excluded.name, listen_host = excluded.listen_host,
            listen_port = excluded.listen_port, target_host = excluded.target_host,
            target_port = excluded.target_port, enabled = excluded.enabled",
        params![
            rule.id.as_str(),
            host.as_str(),
            rule.kind.letter(),
            rule.name,
            rule.listen.host,
            rule.listen.port,
            rule.target.as_ref().map(|t| t.host.clone()),
            rule.target.as_ref().map(|t| t.port),
            rule.enabled,
            next_sort,
        ],
    )?;
    Ok(())
}

impl HostStore {
    /// The rules of a host in their order (what the panel shows), oldest first unless reordered.
    pub fn forward_rules(&self, host: &HostId) -> Result<Vec<ForwardRule>> {
        self.conn
            .prepare(&format!("SELECT {COLUMNS} FROM forward_rules WHERE host_id = ?1 ORDER BY sort, rowid"))?
            .query_map(params![host.as_str()], raw_rule)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(RawRule::into_rule)
            .collect()
    }

    /// Creates the rule for `host`, or updates the one with the same id (name, addresses, kind,
    /// enabled flag; not its position). Fails if the host does not exist.
    pub fn save_forward_rule(&mut self, host: &HostId, rule: &ForwardRule) -> Result<()> {
        let tx = self.conn.transaction()?;
        write_rule(&tx, host, rule)?;
        tx.commit()?;
        Ok(())
    }

    /// Switches a rule on or off (what the toggle on its card saves).
    pub fn set_forward_rule_enabled(&mut self, id: &RuleId, enabled: bool) -> Result<()> {
        let changed =
            self.conn.execute("UPDATE forward_rules SET enabled = ?2 WHERE id = ?1", params![id.as_str(), enabled])?;
        if changed == 0 {
            return Err(StoreError::RuleNotFound(id.clone()));
        }
        Ok(())
    }

    /// Deletes a rule.
    pub fn delete_forward_rule(&mut self, id: &RuleId) -> Result<()> {
        let deleted = self.conn.execute("DELETE FROM forward_rules WHERE id = ?1", params![id.as_str()])?;
        if deleted == 0 {
            return Err(StoreError::RuleNotFound(id.clone()));
        }
        Ok(())
    }

    /// Puts the rules of `host` in the order of `order`; rules that are not listed keep their
    /// relative order after the listed ones.
    pub fn reorder_forward_rules(&mut self, host: &HostId, order: &[RuleId]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let current: Vec<String> = tx
            .prepare("SELECT id FROM forward_rules WHERE host_id = ?1 ORDER BY sort, rowid")?
            .query_map(params![host.as_str()], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let listed: Vec<&str> = order.iter().map(RuleId::as_str).filter(|id| current.iter().any(|c| c == id)).collect();
        let rest = current.iter().map(String::as_str).filter(|id| !listed.contains(id));
        for (position, id) in listed.iter().copied().chain(rest).enumerate() {
            tx.execute("UPDATE forward_rules SET sort = ?2 WHERE id = ?1", params![id, position as i64])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Saves the hosts an import plan creates together with the rules made from their forwardings
    /// (`rules` pair each rule with the host it belongs to), all or nothing.
    pub fn apply_import_with_forwards(&mut self, plan: &ImportPlan, rules: &[(HostId, ForwardRule)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        write_hosts(&tx, &plan.hosts)?;
        for (host, rule) in rules {
            write_rule(&tx, host, rule)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// One rule by id (with its host), if it exists.
    pub fn forward_rule(&self, id: &RuleId) -> Result<Option<(HostId, ForwardRule)>> {
        let found = self
            .conn
            .query_row(
                &format!("SELECT host_id, {COLUMNS} FROM forward_rules WHERE id = ?1"),
                params![id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        RawRule {
                            id: row.get(1)?,
                            kind: row.get(2)?,
                            name: row.get(3)?,
                            listen_host: row.get(4)?,
                            listen_port: row.get(5)?,
                            target_host: row.get(6)?,
                            target_port: row.get(7)?,
                            enabled: row.get(8)?,
                        },
                    ))
                },
            )
            .optional()?;
        found.map(|(host, raw)| Ok((HostId::from_raw(host), raw.into_rule()?))).transpose()
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use shellmate_core::{Host, ImportPlan};

    use super::*;

    fn store_with_host() -> (HostStore, Host) {
        let mut store = HostStore::open_in_memory().unwrap();
        let host = Host::new("db", "db.example.com");
        store.save_host(&host).unwrap();
        (store, host)
    }

    fn rule(kind: ForwardKind, listen: &str, target: &str) -> ForwardRule {
        ForwardRule::parse(RuleId::generate(), "", kind, listen, target, true).unwrap()
    }

    #[test]
    fn rules_round_trip_with_every_field() {
        let (mut store, host) = store_with_host();
        let mut local = rule(ForwardKind::Local, "127.0.0.1:5433", "db.internal:5432");
        local.name = "MySQL 主库".into();
        let remote = rule(ForwardKind::Remote, "0.0.0.0:0", "localhost:3000");
        let mut dynamic = rule(ForwardKind::Dynamic, "[::1]:1080", "");
        dynamic.enabled = false;
        for rule in [&local, &remote, &dynamic] {
            store.save_forward_rule(&host.id, rule).unwrap();
        }
        let loaded = store.forward_rules(&host.id).unwrap();
        assert_eq!(loaded, [local, remote, dynamic], "in the order they were saved, nothing lost");
        assert_eq!(loaded[2].target, None);
        assert_eq!(loaded[1].listen.port, 0, "port 0 of a remote forward is kept");
    }

    #[test]
    fn saving_a_known_rule_updates_it_in_place() {
        let (mut store, host) = store_with_host();
        let mut first = rule(ForwardKind::Local, "5433", "db:5432");
        let second = rule(ForwardKind::Local, "5434", "db:5432");
        store.save_forward_rule(&host.id, &first).unwrap();
        store.save_forward_rule(&host.id, &second).unwrap();
        first.name = "renamed".into();
        first.listen.port = 18080;
        first.enabled = false;
        store.save_forward_rule(&host.id, &first).unwrap();
        let loaded = store.forward_rules(&host.id).unwrap();
        assert_eq!(loaded, [first, second], "updated, not added, and still first");
    }

    #[test]
    fn rules_belong_to_their_host() {
        let (mut store, db) = store_with_host();
        let web = Host::new("web", "web.example.com");
        store.save_host(&web).unwrap();
        store.save_forward_rule(&db.id, &rule(ForwardKind::Local, "1111", "a:1")).unwrap();
        store.save_forward_rule(&web.id, &rule(ForwardKind::Dynamic, "1080", "")).unwrap();
        assert_eq!(store.forward_rules(&db.id).unwrap().len(), 1);
        assert_eq!(store.forward_rules(&web.id).unwrap()[0].kind, ForwardKind::Dynamic);
        assert!(store.forward_rules(&HostId::generate()).unwrap().is_empty());
    }

    #[test]
    fn a_rule_of_an_unknown_host_is_refused() {
        let (mut store, _) = store_with_host();
        let err = store.save_forward_rule(&HostId::generate(), &rule(ForwardKind::Local, "1", "a:1")).unwrap_err();
        assert!(matches!(err, StoreError::HostNotFound(_)), "{err}");
    }

    #[test]
    fn the_enabled_flag_is_saved_by_itself() {
        let (mut store, host) = store_with_host();
        let r = rule(ForwardKind::Local, "5433", "db:5432");
        store.save_forward_rule(&host.id, &r).unwrap();
        store.set_forward_rule_enabled(&r.id, false).unwrap();
        assert!(!store.forward_rules(&host.id).unwrap()[0].enabled);
        store.set_forward_rule_enabled(&r.id, true).unwrap();
        assert!(store.forward_rules(&host.id).unwrap()[0].enabled);
        let err = store.set_forward_rule_enabled(&RuleId::generate(), true).unwrap_err();
        assert!(matches!(err, StoreError::RuleNotFound(_)));
    }

    #[test]
    fn deleting_a_rule_and_a_missing_one() {
        let (mut store, host) = store_with_host();
        let (keep, drop_it) = (rule(ForwardKind::Local, "1111", "a:1"), rule(ForwardKind::Local, "2222", "a:2"));
        store.save_forward_rule(&host.id, &keep).unwrap();
        store.save_forward_rule(&host.id, &drop_it).unwrap();
        store.delete_forward_rule(&drop_it.id).unwrap();
        assert_eq!(store.forward_rules(&host.id).unwrap(), [keep]);
        assert!(matches!(store.delete_forward_rule(&drop_it.id), Err(StoreError::RuleNotFound(_))));
        assert_eq!(store.forward_rule(&drop_it.id).unwrap(), None);
    }

    #[test]
    fn rules_can_be_reordered_and_new_ones_go_last() {
        let (mut store, host) = store_with_host();
        let rules: Vec<_> = (1..=3).map(|n| rule(ForwardKind::Local, &format!("{n}000"), "a:1")).collect();
        for r in &rules {
            store.save_forward_rule(&host.id, r).unwrap();
        }
        let ids =
            |store: &HostStore| store.forward_rules(&host.id).unwrap().into_iter().map(|r| r.id).collect::<Vec<_>>();
        store.reorder_forward_rules(&host.id, &[rules[2].id.clone(), rules[0].id.clone()]).unwrap();
        assert_eq!(
            ids(&store),
            [rules[2].id.clone(), rules[0].id.clone(), rules[1].id.clone()],
            "unlisted ones follow"
        );
        let newest = rule(ForwardKind::Remote, "9000", "localhost:3000");
        store.save_forward_rule(&host.id, &newest).unwrap();
        assert_eq!(ids(&store).last(), Some(&newest.id));
        // Unknown ids in the order are ignored.
        store.reorder_forward_rules(&host.id, &[RuleId::generate(), newest.id.clone()]).unwrap();
        assert_eq!(ids(&store)[0], newest.id);
    }

    #[test]
    fn deleting_a_host_deletes_its_rules() {
        let (mut store, db) = store_with_host();
        let web = Host::new("web", "web.example.com");
        store.save_host(&web).unwrap();
        let doomed = rule(ForwardKind::Local, "1111", "a:1");
        let kept = rule(ForwardKind::Dynamic, "1080", "");
        store.save_forward_rule(&db.id, &doomed).unwrap();
        store.save_forward_rule(&web.id, &kept).unwrap();

        store.delete_host(&db.id).unwrap();
        assert_eq!(store.forward_rule(&doomed.id).unwrap(), None, "the rule went with its host");
        let rows: i64 = store.conn.query_row("SELECT COUNT(*) FROM forward_rules", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "the other host keeps its rule");
        assert_eq!(store.forward_rules(&web.id).unwrap(), [kept]);
    }

    #[test]
    fn foreign_keys_are_enforced_on_every_way_of_opening_the_store() {
        let enforced = |store: &HostStore| -> bool {
            store.conn.pragma_query_value(None, "foreign_keys", |row| row.get(0)).unwrap()
        };
        assert!(enforced(&HostStore::open_in_memory().unwrap()));
        let dir = tempfile::tempdir().unwrap();
        assert!(enforced(&HostStore::open(dir.path().join("a.db")).unwrap()));
        // A rule for a host that does not exist cannot be written behind the store's back either.
        let store = HostStore::open_in_memory().unwrap();
        let err = store
            .conn
            .execute(
                "INSERT INTO forward_rules (id, host_id, kind, name, listen_host, listen_port) VALUES ('r', 'nope', 'D', 'x', '127.0.0.1', 1)",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
    }

    #[test]
    fn a_v2_library_is_upgraded_keeping_hosts_and_settings_and_gets_the_rules_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shellmate.db");
        {
            // What the last version wrote: hosts and settings, user_version 2.
            let mut conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            crate::hosts::migrate_for_tests(&mut conn, 2);
            conn.execute(
                "INSERT INTO hosts (id, name, addr, port, protocol, auth) VALUES ('h1', 'prod', '10.0.0.1', 22, 'ssh', 'agent')",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO settings (key, value) VALUES ('sidebar_collapsed', '1')", []).unwrap();
            let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
            assert_eq!(version, 2);
        }
        let mut store = HostStore::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 3);
        assert_eq!(store.hosts().unwrap().len(), 1, "the host is still there");
        assert_eq!(store.setting("sidebar_collapsed").unwrap().as_deref(), Some("1"));
        let host = store.hosts().unwrap().remove(0);
        assert!(store.forward_rules(&host.id).unwrap().is_empty());
        let r = rule(ForwardKind::Local, "5433", "db:5432");
        store.save_forward_rule(&host.id, &r).unwrap();
        drop(store);
        // Opening it again changes nothing.
        let store = HostStore::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 3);
        assert_eq!(store.forward_rules(&host.id).unwrap(), [r]);
    }

    #[test]
    fn a_damaged_rule_row_is_reported_not_guessed() {
        let (store, host) = store_with_host();
        let cases: [(&str, i64, Option<&str>, Option<i64>); 4] =
            [("X", 1, None, None), ("L", 1, None, None), ("D", 70_000, None, None), ("L", 1, Some("h"), None)];
        for (kind, port, target_host, target_port) in cases {
            store.conn.execute("DELETE FROM forward_rules", []).unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO forward_rules (id, host_id, kind, name, listen_host, listen_port, target_host, target_port) VALUES ('r', ?1, ?2, 'n', '127.0.0.1', ?3, ?4, ?5)",
                    params![host.id.as_str(), kind, port, target_host, target_port],
                )
                .unwrap_or_else(|err| {
                    // The CHECK constraint already refuses an unknown kind.
                    assert_eq!(kind, "X", "{err}");
                    0
                });
            if kind != "X" {
                assert!(matches!(store.forward_rules(&host.id), Err(StoreError::Corrupt(_))), "{kind} {port}");
            }
        }
    }

    #[test]
    fn an_import_saves_hosts_and_their_rules_together_or_not_at_all() {
        let mut store = HostStore::open_in_memory().unwrap();
        let (a, b) = (Host::new("a", "a.example.com"), Host::new("b", "b.example.com"));
        let plan = ImportPlan { hosts: vec![a.clone(), b.clone()], ..Default::default() };
        let (ra, rb) = (rule(ForwardKind::Local, "3306", "a:3306"), rule(ForwardKind::Dynamic, "1080", ""));
        store.apply_import_with_forwards(&plan, &[(a.id.clone(), ra.clone()), (b.id.clone(), rb.clone())]).unwrap();
        assert_eq!(store.forward_rules(&a.id).unwrap(), [ra]);
        assert_eq!(store.forward_rules(&b.id).unwrap(), [rb]);

        let c = Host::new("c", "c.example.com");
        let plan = ImportPlan { hosts: vec![c.clone()], ..Default::default() };
        let stray = (HostId::generate(), rule(ForwardKind::Local, "1", "x:1"));
        assert!(store.apply_import_with_forwards(&plan, &[stray]).is_err());
        assert_eq!(store.host(&c.id).unwrap(), None, "the host of the failed import was not saved either");
    }
}
