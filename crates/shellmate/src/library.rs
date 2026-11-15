//! Changes to the host library that the editor, the delete confirmation and the importer make.
//!
//! These functions work on a [`HostStore`] and a [`SecretStore`] only, so they are tested with
//! an in-memory database and an in-memory secret store. Errors are user-facing Chinese messages.

use std::path::{Path, PathBuf};

use shellmate_core::{
    AuthMethod, ForwardDirective, Group, GroupTone, Host, HostId, ImportCandidate, ImportPlan, Secret, plan_import,
};
use shellmate_forward::{ForwardKind, ForwardRule, RuleId};
use shellmate_store::{HostStore, SecretKey, SecretStore};

/// What the host editor produced.
pub struct HostDraft {
    /// The host to save (new, or the edited copy of a saved one).
    pub host: Host,
    /// Name of the group to put the host in; created if it does not exist yet. Empty: no group.
    pub group_name: String,
    /// A password to store in the keychain.
    pub password: Option<Secret>,
}

/// What a successful save has to tell the user besides "saved".
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SaveOutcome {
    /// The host was saved but its password could not be stored in the keychain.
    pub keychain_warning: Option<String>,
}

/// The group called `name`, created (last in order) if it does not exist.
fn group_named(store: &mut HostStore, name: &str) -> Result<Group, String> {
    let groups = store.groups().map_err(|e| e.to_string())?;
    if let Some(existing) = groups.iter().find(|g| g.name == name) {
        return Ok(existing.clone());
    }
    let mut group = Group::new(name, GroupTone::Neutral);
    group.sort = groups.iter().map(|g| g.sort).max().map_or(0, |s| s + 1);
    store.save_group(&group).map_err(|e| e.to_string())?;
    Ok(group)
}

/// Saves the host the editor produced, creating its group and storing its password.
pub fn save_host(store: &mut HostStore, secrets: &dyn SecretStore, draft: HostDraft) -> Result<SaveOutcome, String> {
    let HostDraft { mut host, group_name, password } = draft;
    let group_name = group_name.trim();
    host.group = if group_name.is_empty() { None } else { Some(group_named(store, group_name)?.id) };
    store.save_host(&host).map_err(|e| e.to_string())?;

    let mut outcome = SaveOutcome::default();
    if let Some(password) = password
        && let Err(err) = secrets.set(&SecretKey::HostPassword(host.id.clone()), &password)
    {
        outcome.keychain_warning = Some(err.to_string());
    }
    Ok(outcome)
}

/// Deletes a host and its keychain password. Refused while other hosts jump through it.
/// Returns a warning if the keychain entry could not be removed.
pub fn delete_host(store: &mut HostStore, secrets: &dyn SecretStore, id: &HostId) -> Result<Option<String>, String> {
    store.delete_host(id).map_err(|e| e.to_string())?;
    Ok(secrets.delete(&SecretKey::HostPassword(id.clone())).err().map(|e| e.to_string()))
}

/// What an import added.
#[derive(Debug, PartialEq, Eq)]
pub struct ImportSummary {
    /// Hosts created for the selected rows.
    pub imported: usize,
    /// Hosts created only because an imported host jumps through them.
    pub jump_hosts: usize,
    /// Selected rows that already existed.
    pub skipped_existing: usize,
    /// Port forwarding rules created from the `LocalForward` / `RemoteForward` / `DynamicForward` lines.
    pub forwards: usize,
    /// Forwardings that could not be taken over (Unix-domain sockets, remote SOCKS, unreadable lines).
    pub skipped_forwards: usize,
}

impl ImportSummary {
    /// `已导入 3 台主机（另创建 1 台跳板机）`
    pub fn message(&self) -> String {
        let mut text = format!("已导入 {} 台主机", self.imported);
        if self.jump_hosts > 0 {
            text.push_str(&format!("（另创建 {} 台跳板机）", self.jump_hosts));
        }
        if self.skipped_existing > 0 {
            text.push_str(&format!("，跳过 {} 台已存在的主机", self.skipped_existing));
        }
        if self.forwards > 0 {
            text.push_str(&format!("，生成 {} 条端口转发规则", self.forwards));
        }
        if self.skipped_forwards > 0 {
            text.push_str(&format!(
                "；另有 {} 条端口转发无法导入（Unix 套接字、远程 SOCKS 或无法识别的写法）",
                self.skipped_forwards
            ));
        }
        text
    }
}

/// Imports the `selected` rows (`all` are all rows of the file, for resolving jump aliases) into
/// the group called `group_name` (created if needed; empty: no group).
pub fn import_hosts(
    store: &mut HostStore,
    selected: &[ImportCandidate],
    all: &[ImportCandidate],
    group_name: &str,
) -> Result<ImportSummary, String> {
    let existing = store.hosts().map_err(|e| e.to_string())?;
    let group_name = group_name.trim();
    let group = if group_name.is_empty() { None } else { Some(group_named(store, group_name)?) };
    let plan = plan_import(selected, all, &existing, group.as_ref().map(|g| &g.id));
    let (rules, unreadable) = forward_rules_of(&plan);
    store.apply_import_with_forwards(&plan, &rules).map_err(|e| e.to_string())?;
    Ok(ImportSummary {
        imported: plan.hosts.len() - plan.jump_hosts,
        jump_hosts: plan.jump_hosts,
        skipped_existing: plan.skipped_existing,
        forwards: rules.len(),
        skipped_forwards: plan.skipped_forwards + unreadable,
    })
}

/// The rules the forwardings of an import become (named like `-L 3306`, enabled), each with its
/// host. A forwarding the rule parser does not accept (a port out of range, a malformed address) is
/// counted in the second number, and a repeat of a rule the host already has is dropped.
pub fn forward_rules_of(plan: &ImportPlan) -> (Vec<(HostId, ForwardRule)>, usize) {
    let mut rules: Vec<(HostId, ForwardRule)> = Vec::new();
    let mut unreadable = 0;
    for (host, forward) in &plan.forwards {
        let kind = match forward.kind {
            ForwardDirective::Local => ForwardKind::Local,
            ForwardDirective::Remote => ForwardKind::Remote,
            ForwardDirective::Dynamic => ForwardKind::Dynamic,
        };
        let target = forward.target.as_deref().unwrap_or_default();
        match ForwardRule::parse(RuleId::generate(), "", kind, &forward.listen, target, true) {
            Ok(rule) => {
                let repeat = rules.iter().any(|(h, r)| h == host && r.kind == rule.kind && r.listen == rule.listen);
                if !repeat {
                    rules.push((host.clone(), rule));
                }
            }
            Err(err) => {
                log::debug!("端口转发 {} 未导入：{err}", forward.listen);
                unreadable += 1;
            }
        }
    }
    (rules, unreadable)
}

// ---- the editor's form ----------------------------------------------------------------

/// The raw text of the host editor's fields.
#[derive(Clone, Debug, Default)]
pub struct EditorFields {
    pub name: String,
    pub addr: String,
    pub port: String,
    pub user: String,
    pub tags: String,
    pub group: String,
    pub identity_file: String,
    pub password: String,
    pub auth: AuthMethod,
    pub jumps: Vec<HostId>,
}

/// Expands a leading `~` against `home`.
pub fn expand_tilde(text: &str, home: Option<&Path>) -> PathBuf {
    match (text.strip_prefix("~/"), text == "~", home) {
        (Some(rest), _, Some(home)) => home.join(rest),
        (None, true, Some(home)) => home.to_path_buf(),
        _ => PathBuf::from(text),
    }
}

/// Validates the form and builds the host to save on top of `original` (which keeps its id,
/// favorite flag, protocol and last connection time) or a new host.
pub fn build_draft(fields: &EditorFields, original: Option<&Host>, home: Option<&Path>) -> Result<HostDraft, String> {
    let name = fields.name.trim();
    if name.is_empty() {
        return Err("请填写名称".into());
    }
    let addr = fields.addr.trim();
    if addr.is_empty() {
        return Err("请填写主机地址".into());
    }
    if addr.chars().any(char::is_whitespace) {
        return Err("主机地址不能包含空格".into());
    }
    let port = match fields.port.trim().parse::<u16>() {
        Ok(port) if port > 0 => port,
        _ => return Err("端口应为 1–65535 之间的数字".into()),
    };

    let mut host = original.cloned().unwrap_or_else(|| Host::new(name, addr));
    host.name = name.to_string();
    host.addr = addr.to_string();
    host.port = port;
    host.user = Some(fields.user.trim().to_string()).filter(|u| !u.is_empty());
    host.auth = fields.auth;
    host.identity_file = match fields.auth {
        AuthMethod::PrivateKey => {
            Some(fields.identity_file.trim()).filter(|p| !p.is_empty()).map(|p| expand_tilde(p, home))
        }
        _ => None,
    };
    let mut tags: Vec<String> = Vec::new();
    for tag in fields.tags.split([',', '，']).map(str::trim).filter(|t| !t.is_empty()) {
        if !tags.iter().any(|t| t == tag) {
            tags.push(tag.to_string());
        }
    }
    host.tags = tags;
    host.jump = fields.jumps.clone();

    let password = Some(fields.password.as_str()).filter(|p| fields.auth == AuthMethod::Password && !p.is_empty());
    Ok(HostDraft { host, group_name: fields.group.clone(), password: password.map(Secret::new) })
}

#[cfg(test)]
mod tests {
    use shellmate_core::{Protocol, parse_ssh_config};
    use shellmate_store::MemoryStore;

    use super::*;

    fn store() -> HostStore {
        HostStore::open_in_memory().unwrap()
    }

    fn fields(name: &str, addr: &str) -> EditorFields {
        EditorFields { name: name.into(), addr: addr.into(), port: "22".into(), ..Default::default() }
    }

    fn draft(fields: &EditorFields) -> HostDraft {
        build_draft(fields, None, Some(Path::new("/Users/me"))).unwrap()
    }

    // ---- the form ----

    #[test]
    fn a_minimal_form_makes_a_default_host() {
        let host = draft(&fields("web", "w.example.com")).host;
        assert_eq!((host.name.as_str(), host.addr.as_str(), host.port), ("web", "w.example.com", 22));
        assert_eq!((host.user, host.auth, host.protocol), (None, AuthMethod::PrivateKey, Protocol::Ssh));
        assert!(host.tags.is_empty() && host.jump.is_empty() && host.identity_file.is_none());
    }

    #[test]
    fn the_form_is_validated_with_clear_messages() {
        let err = |f: EditorFields| build_draft(&f, None, None).err().expect("should be rejected");
        assert_eq!(err(fields("  ", "h")), "请填写名称");
        assert_eq!(err(fields("n", " ")), "请填写主机地址");
        assert_eq!(err(fields("n", "a b")), "主机地址不能包含空格");
        for bad in ["", "0", "70000", "ssh", "-1", "22.5"] {
            assert_eq!(
                err(EditorFields { port: bad.into(), ..fields("n", "h") }),
                "端口应为 1–65535 之间的数字",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn fields_are_trimmed_and_tags_split_on_both_commas() {
        let f = EditorFields {
            name: "  web  ".into(),
            addr: " w.example.com ".into(),
            port: " 2222 ".into(),
            user: "  deploy ".into(),
            tags: "nginx, k8s，nginx ,, 主库 ".into(),
            ..fields("", "")
        };
        let host = draft(&f).host;
        assert_eq!((host.name.as_str(), host.addr.as_str(), host.port), ("web", "w.example.com", 2222));
        assert_eq!(host.user.as_deref(), Some("deploy"));
        assert_eq!(host.tags, ["nginx", "k8s", "主库"], "duplicates and empty tags are dropped");
        assert_eq!(draft(&EditorFields { user: "   ".into(), ..fields("n", "h") }).host.user, None);
    }

    #[test]
    fn the_key_path_only_counts_for_key_authentication() {
        let key =
            EditorFields { auth: AuthMethod::PrivateKey, identity_file: "~/.ssh/id_work".into(), ..fields("n", "h") };
        assert_eq!(draft(&key).host.identity_file, Some(PathBuf::from("/Users/me/.ssh/id_work")));
        let empty = EditorFields { identity_file: "  ".into(), ..key.clone() };
        assert_eq!(draft(&empty).host.identity_file, None, "no path: the default keys are used");
        let agent = EditorFields { auth: AuthMethod::Agent, ..key };
        assert_eq!(draft(&agent).host.identity_file, None, "a leftover path is dropped for other methods");
    }

    #[test]
    fn the_password_is_only_taken_for_password_authentication() {
        let with = |auth, password: &str| {
            draft(&EditorFields { auth, password: password.into(), ..fields("n", "h") }).password
        };
        assert_eq!(with(AuthMethod::Password, "pw").map(|s| s.expose().to_string()), Some("pw".into()));
        assert!(with(AuthMethod::Password, "").is_none(), "an empty field keeps what is saved");
        assert!(with(AuthMethod::Agent, "leftover").is_none());
    }

    #[test]
    fn editing_keeps_what_the_form_does_not_show() {
        let mut saved = Host::new("old", "old.example.com");
        saved.favorite = true;
        saved.last_connected_at = Some(42);
        saved.protocol = Protocol::Mosh;
        let jumps = vec![HostId::generate(), HostId::generate()];
        let f = EditorFields { jumps: jumps.clone(), ..fields("renamed", "new.example.com") };
        let host = build_draft(&f, Some(&saved), None).unwrap().host;
        assert_eq!(
            (host.id.clone(), host.favorite, host.last_connected_at, host.protocol),
            (saved.id, true, Some(42), Protocol::Mosh)
        );
        assert_eq!((host.name.as_str(), host.jump), ("renamed", jumps));
    }

    // ---- saving ----

    #[test]
    fn saving_creates_a_missing_group_and_reuses_an_existing_one() {
        let (mut store, secrets) = (store(), MemoryStore::new());
        let mut first = draft(&fields("a", "a.example.com"));
        first.group_name = " 生产环境 ".into();
        save_host(&mut store, &secrets, first).unwrap();
        let mut second = draft(&fields("b", "b.example.com"));
        second.group_name = "生产环境".into();
        save_host(&mut store, &secrets, second).unwrap();
        let mut third = draft(&fields("c", "c.example.com"));
        third.group_name = "测试".into();
        save_host(&mut store, &secrets, third).unwrap();

        let groups = store.groups().unwrap();
        assert_eq!(
            groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            ["生产环境", "测试"],
            "created in order, none twice"
        );
        let hosts = store.hosts().unwrap();
        assert_eq!(hosts[0].group, hosts[1].group);
        assert_eq!(hosts[2].group.as_ref(), Some(&groups[1].id));
    }

    #[test]
    fn saving_without_a_group_name_ungroups_the_host() {
        let (mut store, secrets) = (store(), MemoryStore::new());
        let mut grouped = draft(&fields("a", "a.example.com"));
        grouped.group_name = "g".into();
        save_host(&mut store, &secrets, grouped).unwrap();
        let mut host = store.hosts().unwrap().remove(0);
        host.name = "a2".into();
        save_host(&mut store, &secrets, HostDraft { host, group_name: "  ".into(), password: None }).unwrap();
        let saved = store.hosts().unwrap().remove(0);
        assert_eq!((saved.name.as_str(), saved.group), ("a2", None));
        assert_eq!(store.hosts().unwrap().len(), 1, "an edit updates in place");
    }

    #[test]
    fn saving_stores_the_password_in_the_secret_store_only() {
        let (mut store, secrets) = (store(), MemoryStore::new());
        let mut d = draft(&EditorFields { auth: AuthMethod::Password, password: "hunter2".into(), ..fields("n", "h") });
        let id = d.host.id.clone();
        d.group_name.clear();
        assert_eq!(save_host(&mut store, &secrets, d).unwrap(), SaveOutcome::default());
        assert_eq!(secrets.get(&SecretKey::HostPassword(id)).unwrap().unwrap().expose(), "hunter2");
    }

    #[test]
    fn a_keychain_failure_does_not_lose_the_host() {
        struct Failing;
        impl SecretStore for Failing {
            fn get(&self, _: &SecretKey) -> Result<Option<Secret>, shellmate_store::SecretError> {
                Ok(None)
            }
            fn set(&self, _: &SecretKey, _: &Secret) -> Result<(), shellmate_store::SecretError> {
                Err(shellmate_store::SecretError("无法访问钥匙串".into()))
            }
            fn delete(&self, _: &SecretKey) -> Result<(), shellmate_store::SecretError> {
                Err(shellmate_store::SecretError("无法访问钥匙串".into()))
            }
        }
        let mut store = store();
        let d = draft(&EditorFields { auth: AuthMethod::Password, password: "pw".into(), ..fields("n", "h") });
        let outcome = save_host(&mut store, &Failing, d).unwrap();
        assert_eq!(outcome.keychain_warning.as_deref(), Some("钥匙串访问失败：无法访问钥匙串"));
        assert_eq!(store.hosts().unwrap().len(), 1);
        let id = store.hosts().unwrap()[0].id.clone();
        let warning = delete_host(&mut store, &Failing, &id).unwrap();
        assert!(warning.is_some() && store.hosts().unwrap().is_empty(), "deleted, with a warning about the keychain");
    }

    #[test]
    fn store_errors_such_as_jump_cycles_come_back_as_messages() {
        let (mut store, secrets) = (store(), MemoryStore::new());
        let a = draft(&fields("a", "a.example.com"));
        let b = draft(&EditorFields { jumps: vec![a.host.id.clone()], ..fields("b", "b.example.com") });
        let (a_id, b_id) = (a.host.id.clone(), b.host.id.clone());
        save_host(&mut store, &secrets, a).unwrap();
        save_host(&mut store, &secrets, b).unwrap();

        let mut looping = store.host(&a_id).unwrap().unwrap();
        looping.jump = vec![b_id];
        let err =
            save_host(&mut store, &secrets, HostDraft { host: looping, group_name: String::new(), password: None })
                .unwrap_err();
        assert!(err.starts_with("跳板机配置形成了循环："), "{err}");
        assert!(store.host(&a_id).unwrap().unwrap().jump.is_empty(), "nothing was saved");
    }

    // ---- deleting ----

    #[test]
    fn deleting_removes_the_host_and_its_keychain_password() {
        let (mut store, secrets) = (store(), MemoryStore::new());
        let d = draft(&EditorFields { auth: AuthMethod::Password, password: "pw".into(), ..fields("n", "h") });
        let id = d.host.id.clone();
        save_host(&mut store, &secrets, d).unwrap();
        let other = SecretKey::KeyPassphrase("/keys/shared".into());
        secrets.set(&other, &Secret::new("kept")).unwrap();

        assert_eq!(delete_host(&mut store, &secrets, &id).unwrap(), None);
        assert!(store.hosts().unwrap().is_empty());
        assert!(secrets.get(&SecretKey::HostPassword(id)).unwrap().is_none());
        assert!(secrets.get(&other).unwrap().is_some(), "key passphrases may be shared by several hosts and stay");
    }

    #[test]
    fn deleting_a_jump_host_in_use_is_refused_and_keeps_its_password() {
        let (mut store, secrets) = (store(), MemoryStore::new());
        let bastion = draft(&EditorFields {
            auth: AuthMethod::Password,
            password: "pw".into(),
            ..fields("bastion", "b.example.com")
        });
        let id = bastion.host.id.clone();
        let db = draft(&EditorFields { jumps: vec![id.clone()], ..fields("db", "d.example.com") });
        save_host(&mut store, &secrets, bastion).unwrap();
        save_host(&mut store, &secrets, db).unwrap();

        let err = delete_host(&mut store, &secrets, &id).unwrap_err();
        assert_eq!(err, "无法删除「bastion」：主机「db」将它用作跳板机");
        assert!(secrets.get(&SecretKey::HostPassword(id)).unwrap().is_some(), "nothing was removed");
    }

    // ---- importing ----

    #[test]
    fn importing_creates_hosts_in_a_new_group_and_reports_what_it_did() {
        let mut store = store();
        let parsed = parse_ssh_config(
            "Host web\n  HostName 10.0.0.1\n  ProxyJump ops@gw.example.com:2200\nHost db\n  HostName 10.0.0.2\n",
            None,
        );
        let rows = shellmate_core::candidates(&parsed, &[]);
        let summary = import_hosts(&mut store, &rows, &rows, "导入自 ssh config").unwrap();
        assert_eq!(
            summary,
            ImportSummary { imported: 2, jump_hosts: 1, skipped_existing: 0, forwards: 0, skipped_forwards: 0 }
        );
        assert_eq!(summary.message(), "已导入 2 台主机（另创建 1 台跳板机）");

        let book = store.book().unwrap();
        assert_eq!(book.groups.len(), 1);
        assert_eq!(book.groups[0].name, "导入自 ssh config");
        assert!(book.hosts.iter().all(|h| h.group.as_ref() == Some(&book.groups[0].id)));
        let web = book.hosts.iter().find(|h| h.name == "web").unwrap();
        assert_eq!(store.resolve_route(&web.id).unwrap().len(), 2);

        // Again: everything exists now.
        let rows = shellmate_core::candidates(&parsed, &store.hosts().unwrap());
        let again = import_hosts(&mut store, &rows, &rows, "导入自 ssh config").unwrap();
        assert_eq!(
            again,
            ImportSummary { imported: 0, jump_hosts: 0, skipped_existing: 2, forwards: 0, skipped_forwards: 0 }
        );
        assert_eq!(again.message(), "已导入 0 台主机，跳过 2 台已存在的主机");
        assert_eq!(store.hosts().unwrap().len(), 3);
    }

    #[test]
    fn importing_only_the_selected_rows() {
        let mut store = store();
        let parsed = parse_ssh_config("Host a\n  HostName a\nHost b\n  HostName b\nHost c\n  HostName c\n", None);
        let rows = shellmate_core::candidates(&parsed, &[]);
        let picked = [rows[0].clone(), rows[2].clone()];
        let summary = import_hosts(&mut store, &picked, &rows, "").unwrap();
        assert_eq!(summary.imported, 2);
        let hosts = store.hosts().unwrap();
        assert_eq!(hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert!(
            hosts.iter().all(|h| h.group.is_none()) && store.groups().unwrap().is_empty(),
            "an empty group name means no group"
        );
    }

    #[test]
    fn importing_turns_forwardings_into_enabled_rules_named_after_them() {
        let mut store = store();
        let parsed = parse_ssh_config(
            "Host db\n  HostName 10.0.2.8\n  LocalForward 3306 10.0.2.8:3306\n  LocalForward [::1]:5432 db.internal:5432\n  RemoteForward 0.0.0.0:9000 localhost:3000\n  DynamicForward *:1080\n  LocalForward 3306 10.0.2.8:3306\n",
            None,
        );
        let rows = shellmate_core::candidates(&parsed, &[]);
        let summary = import_hosts(&mut store, &rows, &rows, "").unwrap();
        assert_eq!((summary.forwards, summary.skipped_forwards), (4, 0), "the repeated line is one rule");
        assert_eq!(summary.message(), "已导入 1 台主机，生成 4 条端口转发规则");

        let db = store.hosts().unwrap().remove(0);
        let rules = store.forward_rules(&db.id).unwrap();
        assert_eq!(
            rules.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["-L 3306", "-L 5432", "-R 9000", "-D 1080"]
        );
        assert!(rules.iter().all(|r| r.enabled));
        assert_eq!(rules[0].route_text(), "127.0.0.1:3306 → 10.0.2.8:3306");
        assert_eq!(rules[1].listen.host, "::1");
        assert_eq!(rules[2].listen.to_string(), "0.0.0.0:9000", "a remote forward's bind address is the server's");
        assert_eq!(rules[3].listen.to_string(), "0.0.0.0:1080", "* binds every address");
    }

    #[test]
    fn what_cannot_be_imported_is_counted_in_the_summary_not_an_error() {
        let mut store = store();
        let parsed = parse_ssh_config(
            "Host db\n  HostName d\n  LocalForward 5432 /var/run/pg.sock\n  RemoteForward 1080\n  LocalForward 99999 h:1\n  LocalForward 8080 h:notaport\n  LocalForward 7000 ok:7000\n",
            None,
        );
        let rows = shellmate_core::candidates(&parsed, &[]);
        let summary = import_hosts(&mut store, &rows, &rows, "").unwrap();
        assert_eq!(
            (summary.forwards, summary.skipped_forwards),
            (1, 4),
            "two skipped by the parser, two refused by the rules"
        );
        assert_eq!(
            summary.message(),
            "已导入 1 台主机，生成 1 条端口转发规则；另有 4 条端口转发无法导入（Unix 套接字、远程 SOCKS 或无法识别的写法）"
        );
        let db = store.hosts().unwrap().remove(0);
        assert_eq!(store.forward_rules(&db.id).unwrap().len(), 1);
    }

    #[test]
    fn hosts_that_exist_keep_their_rules_when_imported_again() {
        let mut store = store();
        let parsed = parse_ssh_config("Host db\n  HostName d\n  LocalForward 3306 d:3306\n", None);
        let rows = shellmate_core::candidates(&parsed, &[]);
        import_hosts(&mut store, &rows, &rows, "").unwrap();
        let rows = shellmate_core::candidates(&parsed, &store.hosts().unwrap());
        let again = import_hosts(&mut store, &rows, &rows, "").unwrap();
        assert_eq!((again.forwards, again.skipped_existing), (0, 1));
        let db = store.hosts().unwrap().remove(0);
        assert_eq!(store.forward_rules(&db.id).unwrap().len(), 1, "no second copy");
    }
}
