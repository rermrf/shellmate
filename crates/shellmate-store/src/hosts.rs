//! SQLite host library: groups, hosts, jump chains.
//!
//! The database holds no secrets. The schema version is kept in
//! `PRAGMA user_version`; opening a database brings it up to date.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use shellmate_core::{AuthMethod, Group, GroupId, GroupTone, Host, HostBook, HostId, ImportPlan, Protocol};

use crate::error::{Result, StoreError};

/// Schema migrations; entry `n` upgrades version `n` to `n + 1`.
const MIGRATIONS: &[&str] = &[
    // v1
    "
    CREATE TABLE groups (
        id    TEXT PRIMARY KEY NOT NULL,
        name  TEXT NOT NULL UNIQUE,
        tone  TEXT NOT NULL,
        sort  INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE hosts (
        id                 TEXT PRIMARY KEY NOT NULL,
        name               TEXT NOT NULL,
        addr               TEXT NOT NULL,
        port               INTEGER NOT NULL DEFAULT 22,
        username           TEXT,
        group_id           TEXT REFERENCES groups(id) ON DELETE SET NULL,
        favorite           INTEGER NOT NULL DEFAULT 0,
        protocol           TEXT NOT NULL,
        auth               TEXT NOT NULL,
        identity_file      TEXT,
        last_connected_at  INTEGER
    );
    CREATE TABLE host_tags (
        host_id   TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
        position  INTEGER NOT NULL,
        tag       TEXT NOT NULL,
        PRIMARY KEY (host_id, position)
    );
    CREATE TABLE host_jumps (
        host_id   TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
        position  INTEGER NOT NULL,
        jump_id   TEXT NOT NULL REFERENCES hosts(id) ON DELETE RESTRICT,
        PRIMARY KEY (host_id, position)
    );
    CREATE INDEX host_jumps_by_jump ON host_jumps(jump_id);
    ",
    // v2: small preferences of the app itself (whether the sidebar is collapsed, …), by name.
    "
    CREATE TABLE settings (
        key    TEXT PRIMARY KEY NOT NULL,
        value  TEXT NOT NULL
    );
    ",
    // v3: port forwarding rules of a host (the deletion of the host takes them along).
    "
    CREATE TABLE forward_rules (
        id           TEXT PRIMARY KEY NOT NULL,
        host_id      TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
        kind         TEXT NOT NULL CHECK (kind IN ('L', 'R', 'D')),
        name         TEXT NOT NULL,
        listen_host  TEXT NOT NULL,
        listen_port  INTEGER NOT NULL,
        target_host  TEXT,
        target_port  INTEGER,
        enabled      INTEGER NOT NULL DEFAULT 1,
        sort         INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX forward_rules_by_host ON forward_rules(host_id, sort);
    ",
];

/// The host library.
pub struct HostStore {
    pub(crate) conn: Connection,
}

impl HostStore {
    /// `~/Library/Application Support/Shellmate/shellmate.db`.
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::home_dir()?;
        Some(home.join("Library").join("Application Support").join("Shellmate").join("shellmate.db"))
    }

    /// Opens (creating if needed) the database at `path` and migrates it.
    /// A new directory gets mode 0700 and the file is kept at 0600.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        prepare_file(path)?;
        Self::from_connection(Connection::open(path)?)
    }

    /// A private database in memory (tests, and the fallback when the file cannot be used).
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn, MIGRATIONS)?;
        Ok(Self { conn })
    }

    /// The schema version of the open database.
    pub fn schema_version(&self) -> Result<i64> {
        user_version(&self.conn)
    }

    // ---- settings --------------------------------------------------------

    /// The value saved under `key`, if any.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| row.get(0)).optional()?)
    }

    /// Saves `value` under `key`, replacing what was there.
    pub fn set_setting(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- groups ----------------------------------------------------------

    /// All groups, ordered by `sort` then name.
    pub fn groups(&self) -> Result<Vec<Group>> {
        load_groups(&self.conn)
    }

    /// Creates the group or updates the one with the same id.
    pub fn save_group(&mut self, group: &Group) -> Result<()> {
        let taken: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM groups WHERE name = ?1 AND id <> ?2)",
            params![group.name, group.id.as_str()],
            |row| row.get(0),
        )?;
        if taken {
            return Err(StoreError::DuplicateGroup(group.name.clone()));
        }
        self.conn.execute(
            "INSERT INTO groups (id, name, tone, sort) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, tone = excluded.tone, sort = excluded.sort",
            params![group.id.as_str(), group.name, tone_to_str(group.tone), group.sort],
        )?;
        Ok(())
    }

    /// Deletes the group; its hosts stay and become ungrouped.
    pub fn delete_group(&mut self, id: &GroupId) -> Result<()> {
        let deleted = self.conn.execute("DELETE FROM groups WHERE id = ?1", params![id.as_str()])?;
        if deleted == 0 {
            return Err(StoreError::GroupNotFound(id.clone()));
        }
        Ok(())
    }

    // ---- hosts -----------------------------------------------------------

    /// All hosts in the order they were created.
    pub fn hosts(&self) -> Result<Vec<Host>> {
        load_hosts(&self.conn, None)
    }

    /// One host.
    pub fn host(&self, id: &HostId) -> Result<Option<Host>> {
        load_host(&self.conn, id)
    }

    /// Groups and hosts together, as the sidebar wants them.
    pub fn book(&self) -> Result<HostBook> {
        Ok(HostBook::new(self.groups()?, self.hosts()?))
    }

    /// Creates the host or updates the one with the same id.
    /// See [`save_hosts`](Self::save_hosts) for the checks.
    pub fn save_host(&mut self, host: &Host) -> Result<()> {
        self.save_hosts(std::slice::from_ref(host))
    }

    /// Creates or updates several hosts atomically. Jump references may point at
    /// saved hosts or at hosts of the same batch.
    ///
    /// Fails (and saves nothing) if a group or jump host does not exist, or if a
    /// host's jump chain would loop back to itself.
    pub fn save_hosts(&mut self, hosts: &[Host]) -> Result<()> {
        let tx = self.conn.transaction()?;
        write_hosts(&tx, hosts)?;
        tx.commit()?;
        Ok(())
    }

    /// Saves the hosts an import plan creates.
    pub fn apply_import(&mut self, plan: &ImportPlan) -> Result<()> {
        self.save_hosts(&plan.hosts)
    }

    /// Deletes a host. Refused while other hosts use it as a jump host.
    /// (Its keychain password is not touched: delete the `SecretKey::HostPassword` too.)
    pub fn delete_host(&mut self, id: &HostId) -> Result<()> {
        let tx = self.conn.transaction()?;
        let name: String = tx
            .query_row("SELECT name FROM hosts WHERE id = ?1", params![id.as_str()], |row| row.get(0))
            .optional()?
            .ok_or_else(|| StoreError::HostNotFound(id.clone()))?;
        let dependents: Vec<String> = tx
            .prepare(
                "SELECT h.name FROM hosts h
                 WHERE h.id <> ?1 AND EXISTS (SELECT 1 FROM host_jumps j WHERE j.host_id = h.id AND j.jump_id = ?1)
                 ORDER BY h.rowid",
            )?
            .query_map(params![id.as_str()], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if !dependents.is_empty() {
            return Err(StoreError::HostInUse { name, dependents });
        }
        tx.execute("DELETE FROM hosts WHERE id = ?1", params![id.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    /// Records a connection to the host just now; returns the timestamp.
    pub fn touch_connected(&mut self, id: &HostId) -> Result<i64> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        self.touch_connected_at(id, now)?;
        Ok(now)
    }

    /// Records a connection to the host at `unix_secs`.
    pub fn touch_connected_at(&mut self, id: &HostId, unix_secs: i64) -> Result<()> {
        let changed = self
            .conn
            .execute("UPDATE hosts SET last_connected_at = ?2 WHERE id = ?1", params![id.as_str(), unix_secs])?;
        if changed == 0 {
            return Err(StoreError::HostNotFound(id.clone()));
        }
        Ok(())
    }

    /// The connection chain for a host: its jump hosts (each preceded by the jump
    /// hosts of its own) and finally the host itself, in connection order.
    /// A host that appears twice in a row is listed once.
    pub fn resolve_route(&self, id: &HostId) -> Result<Vec<Host>> {
        resolve(&self.conn, id)
    }
}

impl std::fmt::Debug for HostStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostStore")
    }
}

// ---- file handling ---------------------------------------------------------

/// Creates the directory (0700) and the file (0600) if missing and tightens a looser file mode.
fn prepare_file(path: &Path) -> Result<()> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| StoreError::Io { path, source }
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty() && !d.exists()) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir).map_err(io(dir))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path).map_err(io(path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(io(path))?;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io(path))?;
        }
    }
    Ok(())
}

// ---- migrations ------------------------------------------------------------

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

/// Applies the migrations the database has not seen yet, each in its own transaction.
fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<()> {
    let supported = migrations.len() as i64;
    let found = user_version(conn)?;
    if found > supported {
        return Err(StoreError::TooNew { found, supported });
    }
    for (index, sql) in migrations.iter().enumerate().skip(found as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// Brings a database to `version` (test helper: what an older release would have left behind).
#[cfg(test)]
pub(crate) fn migrate_for_tests(conn: &mut Connection, version: usize) {
    migrate(conn, &MIGRATIONS[..version]).unwrap();
}

// ---- reading ---------------------------------------------------------------

fn load_groups(conn: &Connection) -> Result<Vec<Group>> {
    let rows = conn
        .prepare("SELECT id, name, tone, sort FROM groups ORDER BY sort, name")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(id, name, tone, sort)| Ok(Group { id: GroupId::from_raw(id), name, tone: tone_from_str(&tone)?, sort }))
        .collect()
}

/// One row of `hosts`, before enums and relations are resolved.
struct RawHost {
    id: String,
    name: String,
    addr: String,
    port: i64,
    username: Option<String>,
    group_id: Option<String>,
    favorite: bool,
    protocol: String,
    auth: String,
    identity_file: Option<String>,
    last_connected_at: Option<i64>,
}

const HOST_COLUMNS: &str =
    "id, name, addr, port, username, group_id, favorite, protocol, auth, identity_file, last_connected_at";

fn raw_host(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawHost> {
    Ok(RawHost {
        id: row.get(0)?,
        name: row.get(1)?,
        addr: row.get(2)?,
        port: row.get(3)?,
        username: row.get(4)?,
        group_id: row.get(5)?,
        favorite: row.get(6)?,
        protocol: row.get(7)?,
        auth: row.get(8)?,
        identity_file: row.get(9)?,
        last_connected_at: row.get(10)?,
    })
}

/// Loads every host (or just `only`) with tags and jumps.
fn load_hosts(conn: &Connection, only: Option<&HostId>) -> Result<Vec<Host>> {
    let filter = if only.is_some() { "WHERE id = ?1" } else { "WHERE ?1 IS NULL" };
    let raws = conn
        .prepare(&format!("SELECT {HOST_COLUMNS} FROM hosts {filter} ORDER BY rowid"))?
        .query_map(params![only.map(HostId::as_str)], raw_host)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut tags = relations(conn, "host_tags", "tag", only)?;
    let mut jumps = relations(conn, "host_jumps", "jump_id", only)?;
    raws.into_iter()
        .map(|raw| {
            let port =
                u16::try_from(raw.port).map_err(|_| StoreError::Corrupt(format!("端口 {} 超出范围", raw.port)))?;
            Ok(Host {
                tags: tags.remove(&raw.id).unwrap_or_default(),
                jump: jumps.remove(&raw.id).unwrap_or_default().into_iter().map(HostId::from_raw).collect(),
                id: HostId::from_raw(raw.id),
                name: raw.name,
                addr: raw.addr,
                port,
                user: raw.username,
                group: raw.group_id.map(GroupId::from_raw),
                favorite: raw.favorite,
                protocol: protocol_from_str(&raw.protocol)?,
                auth: auth_from_str(&raw.auth)?,
                identity_file: raw.identity_file.map(PathBuf::from),
                last_connected_at: raw.last_connected_at,
            })
        })
        .collect()
}

fn load_host(conn: &Connection, id: &HostId) -> Result<Option<Host>> {
    Ok(load_hosts(conn, Some(id))?.pop())
}

/// `host_id -> values in position order` of a relation table.
fn relations(
    conn: &Connection,
    table: &str,
    column: &str,
    only: Option<&HostId>,
) -> Result<HashMap<String, Vec<String>>> {
    let filter = if only.is_some() { "WHERE host_id = ?1" } else { "WHERE ?1 IS NULL" };
    let rows = conn
        .prepare(&format!("SELECT host_id, {column} FROM {table} {filter} ORDER BY host_id, position"))?
        .query_map(params![only.map(HostId::as_str)], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (host, value) in rows {
        map.entry(host).or_default().push(value);
    }
    Ok(map)
}

// ---- writing ---------------------------------------------------------------

/// The checks and writes of [`HostStore::save_hosts`], inside the caller's transaction.
pub(crate) fn write_hosts(tx: &Transaction<'_>, hosts: &[Host]) -> Result<()> {
    let mut known: HashSet<String> =
        tx.prepare("SELECT id FROM hosts")?.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
    known.extend(hosts.iter().map(|h| h.id.as_str().to_string()));
    for host in hosts {
        if let Some(group) = &host.group {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM groups WHERE id = ?1)",
                params![group.as_str()],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StoreError::GroupNotFound(group.clone()));
            }
        }
        if let Some(missing) = host.jump.iter().find(|j| !known.contains(j.as_str())) {
            return Err(StoreError::MissingJump { host: host.name.clone(), jump: missing.clone() });
        }
    }

    // Rows first, then jump links: a link may point at a host later in the batch.
    for host in hosts {
        write_host_row(tx, host)?;
    }
    for host in hosts {
        write_jumps(tx, host)?;
    }
    for host in hosts {
        resolve(tx, &host.id)?;
    }
    Ok(())
}


fn write_host_row(tx: &Transaction<'_>, host: &Host) -> Result<()> {
    tx.execute(
        "INSERT INTO hosts (id, name, addr, port, username, group_id, favorite, protocol, auth, identity_file, last_connected_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name, addr = excluded.addr, port = excluded.port, username = excluded.username,
            group_id = excluded.group_id, favorite = excluded.favorite, protocol = excluded.protocol,
            auth = excluded.auth, identity_file = excluded.identity_file,
            last_connected_at = excluded.last_connected_at",
        params![
            host.id.as_str(),
            host.name,
            host.addr,
            host.port,
            host.user,
            host.group.as_ref().map(GroupId::as_str),
            host.favorite,
            protocol_to_str(host.protocol),
            auth_to_str(host.auth),
            host.identity_file.as_ref().map(|p| p.to_string_lossy().into_owned()),
            host.last_connected_at,
        ],
    )?;
    tx.execute("DELETE FROM host_tags WHERE host_id = ?1", params![host.id.as_str()])?;
    for (position, tag) in host.tags.iter().enumerate() {
        tx.execute(
            "INSERT INTO host_tags (host_id, position, tag) VALUES (?1, ?2, ?3)",
            params![host.id.as_str(), position as i64, tag],
        )?;
    }
    Ok(())
}

fn write_jumps(tx: &Transaction<'_>, host: &Host) -> Result<()> {
    tx.execute("DELETE FROM host_jumps WHERE host_id = ?1", params![host.id.as_str()])?;
    for (position, jump) in host.jump.iter().enumerate() {
        tx.execute(
            "INSERT INTO host_jumps (host_id, position, jump_id) VALUES (?1, ?2, ?3)",
            params![host.id.as_str(), position as i64, jump.as_str()],
        )?;
    }
    Ok(())
}

// ---- jump chains -----------------------------------------------------------

/// Expands `id` into its connection chain; see [`HostStore::resolve_route`].
fn resolve(conn: &Connection, id: &HostId) -> Result<Vec<Host>> {
    let target = load_host(conn, id)?.ok_or_else(|| StoreError::HostNotFound(id.clone()))?;
    let mut chain = Vec::new();
    expand(conn, target, &mut Vec::new(), &mut chain)?;
    Ok(chain)
}

fn expand(conn: &Connection, host: Host, path: &mut Vec<Host>, chain: &mut Vec<Host>) -> Result<()> {
    if let Some(start) = path.iter().position(|h| h.id == host.id) {
        let mut names: Vec<String> = path[start..].iter().map(|h| h.name.clone()).collect();
        names.push(host.name);
        return Err(StoreError::JumpCycle { path: names });
    }
    path.push(host.clone());
    for jump in &host.jump {
        let next = load_host(conn, jump)?
            .ok_or_else(|| StoreError::MissingJump { host: host.name.clone(), jump: jump.clone() })?;
        expand(conn, next, path, chain)?;
    }
    path.pop();
    if chain.last().is_none_or(|last| last.id != host.id) {
        chain.push(host);
    }
    Ok(())
}

// ---- enum <-> text -----------------------------------------------------------

fn tone_to_str(tone: GroupTone) -> &'static str {
    match tone {
        GroupTone::Accent => "accent",
        GroupTone::Warn => "warn",
        GroupTone::Ok => "ok",
        GroupTone::Neutral => "neutral",
    }
}

fn tone_from_str(text: &str) -> Result<GroupTone> {
    Ok(match text {
        "accent" => GroupTone::Accent,
        "warn" => GroupTone::Warn,
        "ok" => GroupTone::Ok,
        "neutral" => GroupTone::Neutral,
        other => return Err(StoreError::Corrupt(format!("未知的分组颜色 {other:?}"))),
    })
}

fn protocol_to_str(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Ssh => "ssh",
        Protocol::Mosh => "mosh",
        Protocol::Telnet => "telnet",
        Protocol::Serial => "serial",
    }
}

fn protocol_from_str(text: &str) -> Result<Protocol> {
    Ok(match text {
        "ssh" => Protocol::Ssh,
        "mosh" => Protocol::Mosh,
        "telnet" => Protocol::Telnet,
        "serial" => Protocol::Serial,
        other => return Err(StoreError::Corrupt(format!("未知的协议 {other:?}"))),
    })
}

fn auth_to_str(auth: AuthMethod) -> &'static str {
    match auth {
        AuthMethod::PrivateKey => "private_key",
        AuthMethod::Password => "password",
        AuthMethod::Agent => "agent",
        AuthMethod::KeyboardInteractive => "keyboard_interactive",
    }
}

fn auth_from_str(text: &str) -> Result<AuthMethod> {
    Ok(match text {
        "private_key" => AuthMethod::PrivateKey,
        "password" => AuthMethod::Password,
        "agent" => AuthMethod::Agent,
        "keyboard_interactive" => AuthMethod::KeyboardInteractive,
        other => return Err(StoreError::Corrupt(format!("未知的认证方式 {other:?}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(conn: &Connection) -> i64 {
        user_version(conn).unwrap()
    }

    #[test]
    fn fresh_database_gets_every_migration() {
        let store = HostStore::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
        for table in ["groups", "hosts", "host_tags", "host_jumps", "settings", "forward_rules"] {
            let exists: bool = store
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "table {table}");
        }
    }

    #[test]
    fn a_v1_library_is_upgraded_keeping_its_hosts_and_gets_the_settings_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrate(&mut conn, &MIGRATIONS[..1]).unwrap();
        assert_eq!(version(&conn), 1);
        conn.execute(
            "INSERT INTO hosts (id, name, addr, port, protocol, auth) VALUES ('h1', 'prod', '10.0.0.1', 22, 'ssh', 'agent')",
            [],
        )
        .unwrap();

        migrate(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(version(&conn), MIGRATIONS.len() as i64);
        let hosts: i64 = conn.query_row("SELECT COUNT(*) FROM hosts", [], |r| r.get(0)).unwrap();
        assert_eq!(hosts, 1, "the library of the old version is untouched");
        conn.execute("INSERT INTO settings (key, value) VALUES ('k', 'v')", []).unwrap();
        // Opening the upgraded file again changes nothing.
        migrate(&mut conn, MIGRATIONS).unwrap();
        let kept: String = conn.query_row("SELECT value FROM settings WHERE key = 'k'", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, "v");
    }

    #[test]
    fn settings_are_kept_by_name_and_replaced_on_save() {
        let mut store = HostStore::open_in_memory().unwrap();
        assert_eq!(store.setting("sidebar_collapsed").unwrap(), None, "nothing saved yet");
        store.set_setting("sidebar_collapsed", "1").unwrap();
        assert_eq!(store.setting("sidebar_collapsed").unwrap().as_deref(), Some("1"));
        store.set_setting("sidebar_collapsed", "0").unwrap();
        store.set_setting("other", "x").unwrap();
        assert_eq!(store.setting("sidebar_collapsed").unwrap().as_deref(), Some("0"), "replaced, not added");
        assert_eq!(store.setting("other").unwrap().as_deref(), Some("x"));
    }

    #[test]
    fn settings_survive_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shellmate.db");
        HostStore::open(&path).unwrap().set_setting("sidebar_collapsed", "1").unwrap();
        let reopened = HostStore::open(&path).unwrap();
        assert_eq!(reopened.setting("sidebar_collapsed").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn migrations_run_in_order_and_only_once() {
        let mut conn = Connection::open_in_memory().unwrap();
        let steps = ["CREATE TABLE a (x)", "INSERT INTO a VALUES (1)"];
        migrate(&mut conn, &steps[..1]).unwrap();
        assert_eq!(version(&conn), 1);

        migrate(&mut conn, &steps).unwrap();
        assert_eq!(version(&conn), 2);
        migrate(&mut conn, &steps).unwrap();
        assert_eq!(version(&conn), 2);
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM a", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "the second step ran exactly once");
    }

    #[test]
    fn a_failing_migration_changes_nothing() {
        let mut conn = Connection::open_in_memory().unwrap();
        let steps = ["CREATE TABLE a (x)", "CREATE TABLE b (y); THIS IS NOT SQL"];
        assert!(matches!(migrate(&mut conn, &steps), Err(StoreError::Db(_))));
        assert_eq!(version(&conn), 1, "step one stays applied, step two is rolled back");
        let b_exists: bool =
            conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'b')", [], |r| r.get(0)).unwrap();
        assert!(!b_exists);
    }

    #[test]
    fn a_newer_database_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();
        let err = migrate(&mut conn, MIGRATIONS).unwrap_err();
        assert!(matches!(err, StoreError::TooNew { found: 5, supported: 3 }), "{err}");
        assert_eq!(err.to_string(), "主机库版本较新（v5），当前 Shellmate 只支持到 v3，请升级后再试");
    }
}
