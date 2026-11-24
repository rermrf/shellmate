//! Host library behaviour through the public API (in-memory database unless the file matters).

use std::path::PathBuf;

use shellmate_core::sample::sample_book;
use shellmate_core::{
    AuthMethod, Group, GroupId, GroupTone, Host, HostFilter, HostId, Protocol, candidates, parse_ssh_config,
    plan_import, sections,
};
use shellmate_store::{HostStore, StoreError};

fn store() -> HostStore {
    HostStore::open_in_memory().unwrap()
}

fn host(name: &str) -> Host {
    Host::new(name, format!("{name}.example.com"))
}

fn with_jumps(name: &str, jumps: &[&Host]) -> Host {
    let mut h = host(name);
    h.jump = jumps.iter().map(|j| j.id.clone()).collect();
    h
}

fn names(hosts: &[Host]) -> Vec<&str> {
    hosts.iter().map(|h| h.name.as_str()).collect()
}

#[test]
fn a_new_database_is_empty() {
    let store = store();
    assert!(store.groups().unwrap().is_empty());
    assert!(store.hosts().unwrap().is_empty());
    assert!(store.book().unwrap().hosts.is_empty());
    assert_eq!(store.host(&HostId::generate()).unwrap(), None);
}

#[test]
fn groups_are_ordered_renamed_and_unique() {
    let mut store = store();
    let mut b = Group::new("bravo", GroupTone::Warn);
    b.sort = 2;
    let mut a = Group::new("alpha", GroupTone::Ok);
    a.sort = 1;
    let mut c = Group::new("charlie", GroupTone::Accent);
    c.sort = 1;
    for group in [&b, &a, &c] {
        store.save_group(group).unwrap();
    }
    let listed = store.groups().unwrap();
    assert_eq!(
        listed.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
        ["alpha", "charlie", "bravo"],
        "by sort, then name"
    );
    assert_eq!(listed[0], a);

    // Updating by id renames in place.
    let mut renamed = b.clone();
    renamed.name = "delta".into();
    renamed.tone = GroupTone::Neutral;
    store.save_group(&renamed).unwrap();
    assert_eq!(store.groups().unwrap().len(), 3);
    assert_eq!(store.groups().unwrap().iter().find(|g| g.id == b.id).unwrap().name, "delta");

    // A second group may not take an existing name; the failed save changes nothing.
    let err = store.save_group(&Group::new("alpha", GroupTone::Warn)).unwrap_err();
    assert!(matches!(&err, StoreError::DuplicateGroup(name) if name == "alpha"), "{err}");
    assert_eq!(err.to_string(), "分组「alpha」已存在");
    assert_eq!(store.groups().unwrap().len(), 3);
    // Keeping its own name is not a duplicate.
    store.save_group(&a).unwrap();
}

#[test]
fn deleting_a_group_ungroups_its_hosts() {
    let mut store = store();
    let group = Group::new("prod", GroupTone::Accent);
    store.save_group(&group).unwrap();
    let mut web = host("web");
    web.group = Some(group.id.clone());
    store.save_host(&web).unwrap();

    store.delete_group(&group.id).unwrap();
    assert!(store.groups().unwrap().is_empty());
    assert_eq!(store.host(&web.id).unwrap().unwrap().group, None);
    assert!(matches!(store.delete_group(&group.id), Err(StoreError::GroupNotFound(_))));
}

#[test]
fn a_host_round_trips_with_every_field() {
    let mut store = store();
    let group = Group::new("生产环境", GroupTone::Accent);
    store.save_group(&group).unwrap();
    let bastion = host("bastion");
    store.save_host(&bastion).unwrap();

    let mut db = Host::new("prod-db-01", "10.0.2.8");
    db.port = 2222;
    db.user = Some("dba".into());
    db.group = Some(group.id.clone());
    db.tags = vec!["mysql".into(), "主库".into(), "a,b".into()];
    db.favorite = true;
    db.protocol = Protocol::Ssh;
    db.auth = AuthMethod::KeyboardInteractive;
    db.identity_file = Some(PathBuf::from("/Users/me/.ssh/id db"));
    db.jump = vec![bastion.id.clone()];
    db.last_connected_at = Some(1_700_000_123);
    store.save_host(&db).unwrap();

    assert_eq!(store.host(&db.id).unwrap().unwrap(), db);
    assert_eq!(store.hosts().unwrap(), [bastion, db]);
}

#[test]
fn saving_again_updates_in_place_and_replaces_tags_and_jumps() {
    let mut store = store();
    let (j1, j2) = (host("j1"), host("j2"));
    store.save_hosts(&[j1.clone(), j2.clone()]).unwrap();
    let mut web = host("web");
    web.tags = vec!["a".into(), "b".into()];
    web.jump = vec![j1.id.clone(), j2.id.clone()];
    store.save_host(&web).unwrap();
    store.save_host(&host("later")).unwrap();

    web.name = "web-renamed".into();
    web.port = 2200;
    web.tags = vec!["z".into()];
    web.jump = vec![j2.id.clone()];
    web.auth = AuthMethod::Agent;
    store.save_host(&web).unwrap();

    let all = store.hosts().unwrap();
    assert_eq!(names(&all), ["j1", "j2", "web-renamed", "later"], "creation order is kept");
    assert_eq!(all[2], web);
}

#[test]
fn saving_with_unknown_references_fails_and_saves_nothing() {
    let mut store = store();
    let mut orphan = host("orphan");
    orphan.group = Some(GroupId::generate());
    assert!(matches!(store.save_host(&orphan), Err(StoreError::GroupNotFound(_))));

    let ghost = host("ghost");
    let dangling = with_jumps("dangling", &[&ghost]);
    let err = store.save_hosts(&[host("fine"), dangling]).unwrap_err();
    assert!(matches!(&err, StoreError::MissingJump { host, jump } if host == "dangling" && *jump == ghost.id), "{err}");
    assert!(err.to_string().starts_with("主机「dangling」引用的跳板机不存在"));
    assert!(store.hosts().unwrap().is_empty(), "the batch is atomic");
}

#[test]
fn touch_connected_records_the_time() {
    let mut store = store();
    let web = host("web");
    store.save_host(&web).unwrap();
    assert_eq!(store.host(&web.id).unwrap().unwrap().last_connected_at, None);

    store.touch_connected_at(&web.id, 1_700_000_000).unwrap();
    assert_eq!(store.host(&web.id).unwrap().unwrap().last_connected_at, Some(1_700_000_000));

    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let stamped = store.touch_connected(&web.id).unwrap();
    assert!((before..=before + 5).contains(&stamped), "{stamped} vs {before}");
    assert_eq!(store.host(&web.id).unwrap().unwrap().last_connected_at, Some(stamped));

    assert!(matches!(store.touch_connected(&HostId::generate()), Err(StoreError::HostNotFound(_))));
}

#[test]
fn touched_hosts_show_up_as_recent() {
    let mut store = store();
    let (old, new, never) = (host("old"), host("new"), host("never"));
    store.save_hosts(&[old.clone(), new.clone(), never]).unwrap();
    store.touch_connected_at(&old.id, 100).unwrap();
    store.touch_connected_at(&new.id, 200).unwrap();

    let book = store.book().unwrap();
    let recent = sections(&book, HostFilter::Recent, "");
    assert_eq!(recent[0].hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["new", "old"]);
}

#[test]
fn resolve_route_of_a_plain_host_is_just_the_host() {
    let mut store = store();
    let web = host("web");
    store.save_host(&web).unwrap();
    assert_eq!(store.resolve_route(&web.id).unwrap(), [web]);
    assert!(matches!(store.resolve_route(&HostId::generate()), Err(StoreError::HostNotFound(_))));
}

#[test]
fn resolve_route_lists_jump_hosts_in_connection_order() {
    let mut store = store();
    let (first, second) = (host("first"), host("second"));
    let target = with_jumps("target", &[&first, &second]);
    store.save_hosts(&[target.clone(), first, second]).unwrap();
    assert_eq!(names(&store.resolve_route(&target.id).unwrap()), ["first", "second", "target"]);
}

#[test]
fn resolve_route_expands_nested_jump_hosts() {
    let mut store = store();
    let outer = host("outer");
    let inner = with_jumps("inner", &[&outer]);
    let target = with_jumps("target", &[&inner]);
    store.save_hosts(&[target.clone(), inner, outer]).unwrap();
    assert_eq!(names(&store.resolve_route(&target.id).unwrap()), ["outer", "inner", "target"]);
}

#[test]
fn resolve_route_does_not_repeat_a_host_twice_in_a_row() {
    let mut store = store();
    let a = host("a");
    let b = with_jumps("b", &[&a]);
    // target jumps through a, then b — but b already goes through a.
    let target = with_jumps("target", &[&a, &b]);
    store.save_hosts(&[target.clone(), b, a]).unwrap();
    assert_eq!(names(&store.resolve_route(&target.id).unwrap()), ["a", "b", "target"]);
}

#[test]
fn jump_cycles_are_rejected_when_saving() {
    let mut store = store();
    let (mut a, mut b) = (host("a"), host("b"));
    a.jump = vec![b.id.clone()];
    b.jump = vec![a.id.clone()];
    let err = store.save_hosts(&[a.clone(), b.clone()]).unwrap_err();
    assert!(matches!(&err, StoreError::JumpCycle { path } if path.first() == path.last() && path.len() == 3), "{err}");
    assert!(err.to_string().starts_with("跳板机配置形成了循环："));
    assert!(store.hosts().unwrap().is_empty(), "nothing was saved");

    // Closing a cycle through an edit is refused too, and the stored host is unchanged.
    b.jump.clear();
    store.save_hosts(&[a.clone(), b.clone()]).unwrap();
    let mut b_loop = b.clone();
    b_loop.jump = vec![a.id.clone()];
    assert!(matches!(store.save_host(&b_loop), Err(StoreError::JumpCycle { .. })));
    assert!(store.host(&b.id).unwrap().unwrap().jump.is_empty());

    // A host cannot jump through itself.
    let mut selfish = host("selfish");
    selfish.jump = vec![selfish.id.clone()];
    let err = store.save_host(&selfish).unwrap_err();
    assert_eq!(err.to_string(), "跳板机配置形成了循环：selfish → selfish");
}

#[test]
fn resolve_route_reports_cycles_and_missing_references_found_in_the_file() {
    // Damage the file behind the store's back, with foreign key checks off.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hosts.db");
    let (a, b, ghost) = (host("a"), host("b"), host("ghost"));
    let mut dangling = host("dangling");
    {
        let mut store = HostStore::open(&path).unwrap();
        dangling.jump = vec![ghost.id.clone()];
        store.save_hosts(&[ghost.clone(), dangling.clone(), a.clone(), b.clone()]).unwrap();
    }
    {
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.pragma_update(None, "foreign_keys", false).unwrap();
        raw.execute("DELETE FROM hosts WHERE id = ?1", [ghost.id.as_str()]).unwrap();
        raw.execute("INSERT INTO host_jumps VALUES (?1, 0, ?2)", [a.id.as_str(), b.id.as_str()]).unwrap();
        raw.execute("INSERT INTO host_jumps VALUES (?1, 0, ?2)", [b.id.as_str(), a.id.as_str()]).unwrap();
    }
    let store = HostStore::open(&path).unwrap();
    let err = store.resolve_route(&dangling.id).unwrap_err();
    assert!(matches!(&err, StoreError::MissingJump { host, jump } if host == "dangling" && *jump == ghost.id), "{err}");
    let err = store.resolve_route(&a.id).unwrap_err();
    assert_eq!(err.to_string(), "跳板机配置形成了循环：a → b → a");
}

#[test]
fn a_host_used_as_a_jump_host_cannot_be_deleted() {
    let mut store = store();
    let bastion = host("bastion");
    let (db1, db2) = (with_jumps("db-1", &[&bastion]), with_jumps("db-2", &[&bastion]));
    store.save_hosts(&[bastion.clone(), db1.clone(), db2.clone()]).unwrap();

    let err = store.delete_host(&bastion.id).unwrap_err();
    assert!(
        matches!(&err, StoreError::HostInUse { name, dependents } if name == "bastion" && dependents == &["db-1", "db-2"]),
        "{err}"
    );
    assert_eq!(err.to_string(), "无法删除「bastion」：主机「db-1」、「db-2」将它用作跳板机");
    assert_eq!(store.hosts().unwrap().len(), 3, "nothing was deleted");

    // Free one dependent after the other.
    let mut db1_free = db1.clone();
    db1_free.jump.clear();
    store.save_host(&db1_free).unwrap();
    let err = store.delete_host(&bastion.id).unwrap_err();
    assert!(matches!(&err, StoreError::HostInUse { dependents, .. } if dependents == &["db-2"]));

    store.delete_host(&db2.id).unwrap();
    store.delete_host(&bastion.id).unwrap();
    assert_eq!(names(&store.hosts().unwrap()), ["db-1"]);
}

#[test]
fn deleting_a_host_removes_its_tags_and_links_and_unknown_hosts_are_reported() {
    let mut store = store();
    let jump = host("jump");
    let mut web = with_jumps("web", &[&jump]);
    web.tags = vec!["x".into()];
    store.save_hosts(&[jump.clone(), web.clone()]).unwrap();

    store.delete_host(&web.id).unwrap();
    // The jump host is free again and no stale tag/link rows are left for a re-created host.
    web.tags.clear();
    web.jump.clear();
    store.save_host(&web).unwrap();
    assert_eq!(store.host(&web.id).unwrap().unwrap(), web);
    store.delete_host(&jump.id).unwrap();
    assert!(matches!(store.delete_host(&jump.id), Err(StoreError::HostNotFound(_))));
}

#[test]
fn data_survives_reopening_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hosts.db");
    let group = Group::new("g", GroupTone::Ok);
    let mut web = host("web");
    web.group = Some(group.id.clone());
    {
        let mut store = HostStore::open(&path).unwrap();
        store.save_group(&group).unwrap();
        store.save_host(&web).unwrap();
        store.touch_connected_at(&web.id, 42).unwrap();
    }
    let store = HostStore::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), 10);
    assert_eq!(store.groups().unwrap(), [group]);
    web.last_connected_at = Some(42);
    assert_eq!(store.hosts().unwrap(), [web]);
}

#[test]
fn the_sample_book_round_trips() {
    let mut store = store();
    let sample = sample_book();
    for group in &sample.groups {
        store.save_group(group).unwrap();
    }
    store.save_hosts(&sample.hosts).unwrap();
    let loaded = store.book().unwrap();
    assert_eq!(loaded.groups, sample.groups);
    assert_eq!(loaded.hosts, sample.hosts);
    let db = sample.hosts.iter().find(|h| h.name == "prod-db-01").unwrap();
    assert_eq!(names(&store.resolve_route(&db.id).unwrap()), ["bastion", "prod-db-01"]);
}

#[test]
fn import_end_to_end_dedupes_and_links_jumps() {
    let mut store = store();
    let config = "\
Host bastion
  HostName bastion.corp.io
  Port 2222
  User ops
Host db
  HostName 10.0.2.8
  ProxyJump bastion
Host web
  HostName 10.0.1.21
  ProxyJump ops@gateway.example.com:2200
Host stuff-*
  User nobody
Host *
  User admin
";
    let parsed = parse_ssh_config(config, Some(std::path::Path::new("/Users/me")));
    let rows = candidates(&parsed, &store.hosts().unwrap());
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.existing.is_none()));
    store.apply_import(&plan_import(&rows, &rows, &store.hosts().unwrap(), None)).unwrap();

    let hosts = store.hosts().unwrap();
    assert_eq!(hosts.len(), 4, "bastion, db, web and the minimal jump host gateway.example.com");
    let db = hosts.iter().find(|h| h.name == "db").unwrap();
    assert_eq!(db.user.as_deref(), Some("admin"), "Host * default applied");
    let route = store.resolve_route(&db.id).unwrap();
    assert_eq!(names(&route), ["bastion", "db"]);
    assert_eq!((route[0].port, route[0].user.as_deref()), (2222, Some("ops")));
    let web = hosts.iter().find(|h| h.name == "web").unwrap();
    let gateway = &store.resolve_route(&web.id).unwrap()[0];
    assert_eq!(
        (gateway.addr.as_str(), gateway.port, gateway.user.as_deref()),
        ("gateway.example.com", 2200, Some("ops"))
    );

    // Importing the same file again finds everything it already imported.
    let again = candidates(&parsed, &hosts);
    assert!(again.iter().all(|r| r.existing.is_some()), "{again:?}");
    let plan = plan_import(&again, &again, &hosts, None);
    assert!(plan.hosts.is_empty());
    assert_eq!(plan.skipped_existing, 3);
    store.apply_import(&plan).unwrap();
    assert_eq!(store.hosts().unwrap().len(), 4);
}

#[test]
fn import_into_a_group() {
    let mut store = store();
    let group = Group::new("imported", GroupTone::Neutral);
    store.save_group(&group).unwrap();
    let parsed = parse_ssh_config("Host a\n  HostName a.example.com\n", None);
    let rows = candidates(&parsed, &[]);
    store.apply_import(&plan_import(&rows, &rows, &[], Some(&group.id))).unwrap();
    assert_eq!(store.hosts().unwrap()[0].group, Some(group.id));
}

#[test]
fn the_database_has_no_place_for_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hosts.db");
    let mut store = HostStore::open(&path).unwrap();
    let mut web = host("web");
    web.auth = AuthMethod::Password;
    web.identity_file = Some(PathBuf::from("/keys/web"));
    store.save_host(&web).unwrap();
    drop(store);

    let raw = rusqlite::Connection::open(&path).unwrap();
    let tables: Vec<String> = raw
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for table in tables {
        let columns: Vec<String> = raw
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for column in columns {
            let lower = column.to_lowercase();
            assert!(
                !["password", "passphrase", "secret", "token"].iter().any(|w| lower.contains(w)),
                "{table}.{column}"
            );
        }
    }
}

#[cfg(unix)]
mod permissions {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_directory_is_private_and_file_is_0600() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("Application Support").join("Shellmate").join("shellmate.db");
        let mut store = HostStore::open(&path).unwrap();
        store.save_host(&host("web")).unwrap();
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap().parent().unwrap()), 0o700, "every directory it had to create");
    }

    #[test]
    fn existing_directory_is_left_alone_and_a_loose_file_is_tightened() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = root.path().join("shellmate.db");
        drop(HostStore::open(&path).unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut store = HostStore::open(&path).unwrap();
        store.save_host(&host("web")).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(root.path()), 0o755);
        assert_eq!(store.hosts().unwrap().len(), 1, "the data is still there");
    }
}

#[test]
fn a_newer_file_is_refused_with_a_clear_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hosts.db");
    drop(HostStore::open(&path).unwrap());
    rusqlite::Connection::open(&path).unwrap().pragma_update(None, "user_version", 11).unwrap();
    let err = HostStore::open(&path).unwrap_err();
    assert!(matches!(err, StoreError::TooNew { found: 11, .. }), "{err}");
}

#[test]
fn an_unusable_path_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, "x").unwrap();
    let err = HostStore::open(blocker.join("sub").join("hosts.db")).unwrap_err();
    assert!(matches!(err, StoreError::Io { .. }), "{err}");
    assert!(err.to_string().starts_with("无法创建主机库"));
}
