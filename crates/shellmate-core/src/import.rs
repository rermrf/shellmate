//! Importing hosts parsed from an ssh config file into the host book.
//!
//! [`candidates`] turns the parsed entries into rows for the import dialog
//! (marking the ones that already exist); [`plan_import`] decides which hosts
//! to create for the rows the user selected and links `ProxyJump` references.
//! Both are pure; persisting the plan is up to the store.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::host::{AuthMethod, GroupId, Host, HostId};
use crate::ssh_config::ParsedConfig;

/// One row of the import dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportCandidate {
    /// The `Host` alias; becomes the host's name.
    pub alias: String,
    /// Address to connect to: `HostName`, or the alias when there is none.
    pub hostname: String,
    /// `User`, if any.
    pub user: Option<String>,
    /// `Port` (22 by default).
    pub port: u16,
    /// `IdentityFile`, if any.
    pub identity_file: Option<PathBuf>,
    /// `ProxyJump` entries: aliases or `[user@]host[:port]` specs.
    pub proxy_jump: Vec<String>,
    /// The saved host this entry duplicates: same name as the alias, or the same
    /// (address, port, user). Existing entries are not imported again.
    pub existing: Option<HostId>,
}

/// Builds the import rows for `parsed`, flagging those already in `hosts`.
pub fn candidates(parsed: &ParsedConfig, hosts: &[Host]) -> Vec<ImportCandidate> {
    parsed
        .entries
        .iter()
        .map(|entry| {
            let hostname = entry.hostname.clone().unwrap_or_else(|| entry.alias.clone());
            let port = entry.port.unwrap_or(22);
            let existing = hosts
                .iter()
                .find(|h| h.name == entry.alias)
                .or_else(|| hosts.iter().find(|h| h.addr == hostname && h.port == port && h.user == entry.user))
                .map(|h| h.id.clone());
            ImportCandidate {
                alias: entry.alias.clone(),
                hostname,
                user: entry.user.clone(),
                port,
                identity_file: entry.identity_file.clone(),
                proxy_jump: entry.proxy_jump.clone(),
                existing,
            }
        })
        .collect()
}

/// What an import will add.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportPlan {
    /// New hosts to save. Jump references point at ids that are either already
    /// saved or part of this list.
    pub hosts: Vec<Host>,
    /// How many selected rows were skipped because they already exist.
    pub skipped_existing: usize,
    /// How many of `hosts` were created only because a selected host jumps through them.
    pub jump_hosts: usize,
}

/// Decides which hosts to create for the `selected` rows.
///
/// `all` are all rows of the file (jump aliases are looked up there, even when
/// the user did not select them) and `existing` the hosts already saved.
/// A `ProxyJump` entry is linked to, in this order: a host created by this import
/// or already saved under that name; a row of `all` with that alias (created as a
/// jump host if needed); a saved or created host with the same address, port and
/// (if given) user; otherwise a minimal host is created from the
/// `[user@]host[:port]` spec. Links that would form a cycle are dropped.
pub fn plan_import(
    selected: &[ImportCandidate],
    all: &[ImportCandidate],
    existing: &[Host],
    group: Option<&GroupId>,
) -> ImportPlan {
    let mut planner =
        Planner { all, existing, group, created: Vec::new(), by_alias: HashMap::new(), in_progress: Vec::new() };
    let mut plan = ImportPlan::default();
    for candidate in selected {
        if candidate.existing.is_some() {
            plan.skipped_existing += 1;
        } else {
            planner.create_from_candidate(candidate);
        }
    }
    let selected_count = selected.iter().filter(|c| c.existing.is_none()).count();
    plan.jump_hosts = planner.created.len().saturating_sub(selected_count);
    plan.hosts = planner.created;
    plan
}

struct Planner<'a> {
    all: &'a [ImportCandidate],
    existing: &'a [Host],
    group: Option<&'a GroupId>,
    created: Vec<Host>,
    /// Alias of a created host → its index in `created`.
    by_alias: HashMap<String, usize>,
    /// Aliases whose jumps are being resolved, to cut cycles.
    in_progress: Vec<String>,
}

impl Planner<'_> {
    /// Creates the host for a row (once) and resolves its jumps. Returns its id.
    fn create_from_candidate(&mut self, candidate: &ImportCandidate) -> HostId {
        if let Some(&index) = self.by_alias.get(&candidate.alias) {
            return self.created[index].id.clone();
        }
        let mut host = Host::new(candidate.alias.clone(), candidate.hostname.clone());
        host.port = candidate.port;
        host.user = candidate.user.clone();
        host.identity_file = candidate.identity_file.clone();
        host.auth = import_auth(host.identity_file.is_some());
        host.group = self.group.cloned();
        let id = host.id.clone();
        self.by_alias.insert(candidate.alias.clone(), self.created.len());
        let index = self.created.len();
        self.created.push(host);

        self.in_progress.push(candidate.alias.clone());
        let jumps: Vec<HostId> = candidate.proxy_jump.iter().filter_map(|spec| self.resolve_jump(spec)).collect();
        self.in_progress.pop();
        self.created[index].jump = jumps;
        id
    }

    fn resolve_jump(&mut self, spec: &str) -> Option<HostId> {
        // A host created by this import, or saved under that name.
        if let Some(&index) = self.by_alias.get(spec) {
            return self.alias_link(spec, self.created[index].id.clone());
        }
        if let Some(host) = self.existing.iter().find(|h| h.name == spec) {
            return Some(host.id.clone());
        }
        // A row of the file with that alias.
        if let Some(row) = self.all.iter().find(|c| c.alias == spec) {
            return match &row.existing {
                Some(id) => Some(id.clone()),
                None if self.in_progress.iter().any(|a| a == spec) => None,
                None => Some(self.create_from_candidate(row)),
            };
        }
        // A `[user@]host[:port]` spec.
        let (user, host, port) = parse_jump_spec(spec);
        let port = port.unwrap_or(22);
        let same = |h: &Host| h.addr == host && h.port == port && user.is_none_or(|u| h.user.as_deref() == Some(u));
        if let Some(found) = self.created.iter().find(|h| same(h)).or_else(|| self.existing.iter().find(|h| same(h))) {
            return Some(found.id.clone());
        }
        let mut jump = Host::new(host, host);
        jump.auth = import_auth(false);
        jump.port = port;
        jump.user = user.map(String::from);
        jump.group = self.group.cloned();
        let id = jump.id.clone();
        self.created.push(jump);
        Some(id)
    }

    /// Links to a host created in this import unless that would close a cycle.
    fn alias_link(&self, alias: &str, id: HostId) -> Option<HostId> {
        (!self.in_progress.iter().any(|a| a == alias)).then_some(id)
    }
}

/// Authentication of an imported host: its `IdentityFile` if it has one, otherwise
/// ssh-agent (what OpenSSH would try first).
fn import_auth(has_identity_file: bool) -> AuthMethod {
    if has_identity_file { AuthMethod::PrivateKey } else { AuthMethod::Agent }
}

/// Splits `[user@]host[:port]` (a bracketed `[v6]` host is supported).
fn parse_jump_spec(spec: &str) -> (Option<&str>, &str, Option<u16>) {
    let (user, rest) = match spec.rsplit_once('@') {
        Some((user, rest)) if !user.is_empty() => (Some(user), rest),
        _ => (None, spec),
    };
    if let Some(inner) = rest.strip_prefix('[')
        && let Some((host, tail)) = inner.split_once(']')
    {
        return (user, host, tail.strip_prefix(':').and_then(|p| p.parse().ok()));
    }
    match rest.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => match port.parse() {
            Ok(port) => (user, host, Some(port)),
            Err(_) => (user, rest, None),
        },
        _ => (user, rest, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_config::parse_ssh_config;

    fn rows(config: &str, existing: &[Host]) -> Vec<ImportCandidate> {
        candidates(&parse_ssh_config(config, None), existing)
    }

    fn by_name<'a>(plan: &'a ImportPlan, name: &str) -> &'a Host {
        plan.hosts.iter().find(|h| h.name == name).unwrap_or_else(|| panic!("no host {name}"))
    }

    fn saved(name: &str, addr: &str, port: u16, user: Option<&str>) -> Host {
        let mut host = Host::new(name, addr);
        host.port = port;
        host.user = user.map(String::from);
        host
    }

    #[test]
    fn candidates_resolve_defaults() {
        let rows = rows("Host plain\nHost web\n  HostName 10.0.0.1\n  Port 2222\n  User deploy\n", &[]);
        assert_eq!((rows[0].alias.as_str(), rows[0].hostname.as_str(), rows[0].port), ("plain", "plain", 22));
        assert_eq!(
            (rows[1].hostname.as_str(), rows[1].port, rows[1].user.as_deref()),
            ("10.0.0.1", 2222, Some("deploy"))
        );
        assert!(rows.iter().all(|r| r.existing.is_none()));
    }

    #[test]
    fn existing_means_same_name_or_same_endpoint() {
        let by_name_host = saved("web", "other.example.com", 22, None);
        let by_endpoint = saved("renamed", "10.0.0.2", 2222, Some("deploy"));
        let wrong_user = saved("x", "10.0.0.3", 22, Some("alice"));
        let hosts = [by_name_host.clone(), by_endpoint.clone(), wrong_user];
        let rows = rows(
            "Host web\n  HostName 10.9.9.9\nHost db\n  HostName 10.0.0.2\n  Port 2222\n  User deploy\nHost cache\n  HostName 10.0.0.3\n  User bob\nHost fresh\n  HostName 10.0.0.4\n",
            &hosts,
        );
        assert_eq!(rows[0].existing.as_ref(), Some(&by_name_host.id), "same alias");
        assert_eq!(rows[1].existing.as_ref(), Some(&by_endpoint.id), "same hostname, port and user");
        assert_eq!(rows[2].existing, None, "a different user is a different host");
        assert_eq!(rows[3].existing, None);
    }

    #[test]
    fn existing_rows_are_skipped_and_counted() {
        let hosts = [saved("web", "w", 22, None)];
        let rows = rows("Host web\nHost new\n  HostName n\n", &hosts);
        let plan = plan_import(&rows, &rows, &hosts, None);
        assert_eq!(plan.skipped_existing, 1);
        assert_eq!(plan.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["new"]);
        assert_eq!(plan.jump_hosts, 0);
    }

    #[test]
    fn created_hosts_carry_the_settings_and_group() {
        let group = GroupId::generate();
        let rows = rows("Host web\n  HostName 10.0.0.1\n  User deploy\n  Port 2022\n  IdentityFile /keys/web\n", &[]);
        let plan = plan_import(&rows, &rows, &[], Some(&group));
        let host = by_name(&plan, "web");
        assert_eq!((host.addr.as_str(), host.port, host.user.as_deref()), ("10.0.0.1", 2022, Some("deploy")));
        assert_eq!(host.identity_file, Some(PathBuf::from("/keys/web")));
        assert_eq!(
            (host.auth, host.group.as_ref(), host.protocol),
            (AuthMethod::PrivateKey, Some(&group), crate::host::Protocol::Ssh)
        );
        assert!(host.jump.is_empty() && host.last_connected_at.is_none());
    }

    #[test]
    fn hosts_without_an_identity_file_use_the_agent() {
        let rows = rows("Host plain\n  HostName p\nHost keyed\n  HostName k\n  IdentityFile /keys/k\n", &[]);
        let plan = plan_import(&rows, &rows, &[], None);
        assert_eq!(by_name(&plan, "plain").auth, AuthMethod::Agent);
        assert_eq!(by_name(&plan, "keyed").auth, AuthMethod::PrivateKey);
    }

    #[test]
    fn jump_alias_imported_in_the_same_batch_is_linked() {
        let rows =
            rows("Host bastion\n  HostName b.example.com\nHost db\n  HostName 10.0.2.8\n  ProxyJump bastion\n", &[]);
        let plan = plan_import(&rows, &rows, &[], None);
        assert_eq!(plan.hosts.len(), 2);
        assert_eq!(by_name(&plan, "db").jump, [by_name(&plan, "bastion").id.clone()]);
        assert_eq!(plan.jump_hosts, 0, "bastion was selected itself");
    }

    #[test]
    fn jump_alias_of_an_existing_host_is_linked() {
        let bastion = saved("bastion", "b.example.com", 22, None);
        let rows = rows(
            "Host bastion\n  HostName b.example.com\nHost db\n  ProxyJump bastion\n",
            std::slice::from_ref(&bastion),
        );
        let plan = plan_import(&rows, &rows, std::slice::from_ref(&bastion), None);
        assert_eq!(plan.hosts.len(), 1, "bastion exists, only db is created");
        assert_eq!(by_name(&plan, "db").jump, [bastion.id]);
    }

    #[test]
    fn unselected_jump_alias_is_created_with_its_own_settings() {
        let rows = rows(
            "Host bastion\n  HostName b.example.com\n  User ops\n  Port 2222\nHost db\n  ProxyJump bastion\n",
            &[],
        );
        let selected = [rows[1].clone()];
        let plan = plan_import(&selected, &rows, &[], None);
        assert_eq!(plan.hosts.len(), 2);
        assert_eq!(plan.jump_hosts, 1);
        let bastion = by_name(&plan, "bastion");
        assert_eq!(
            (bastion.addr.as_str(), bastion.port, bastion.user.as_deref()),
            ("b.example.com", 2222, Some("ops"))
        );
        assert_eq!(by_name(&plan, "db").jump, std::slice::from_ref(&bastion.id));
    }

    #[test]
    fn jump_chains_keep_their_order_and_nest() {
        let rows = rows(
            "Host a\n  HostName a.example.com\nHost b\n  HostName b.example.com\n  ProxyJump a\nHost target\n  HostName t\n  ProxyJump a,b\n",
            &[],
        );
        let plan = plan_import(&rows, &rows, &[], None);
        let (a, b) = (by_name(&plan, "a").id.clone(), by_name(&plan, "b").id.clone());
        assert_eq!(by_name(&plan, "target").jump, [a.clone(), b]);
        assert_eq!(by_name(&plan, "b").jump, [a]);
    }

    #[test]
    fn spec_matching_nothing_creates_a_minimal_jump_host() {
        let rows = rows("Host db\n  HostName 10.0.2.8\n  ProxyJump ops@jump.example.com:2222\n", &[]);
        let group = GroupId::generate();
        let plan = plan_import(&rows, &rows, &[], Some(&group));
        assert_eq!(plan.hosts.len(), 2);
        assert_eq!(plan.jump_hosts, 1);
        let jump = plan.hosts.iter().find(|h| h.addr == "jump.example.com").unwrap();
        assert_eq!((jump.name.as_str(), jump.port, jump.user.as_deref()), ("jump.example.com", 2222, Some("ops")));
        assert_eq!((jump.auth, jump.group.as_ref()), (AuthMethod::Agent, Some(&group)), "no key file known: ssh-agent");
        assert_eq!(by_name(&plan, "db").jump, std::slice::from_ref(&jump.id));
    }

    #[test]
    fn spec_matching_a_host_by_endpoint_reuses_it() {
        let known = saved("gateway", "gw.example.com", 2222, Some("ops"));
        let rows = rows(
            "Host a\n  HostName a\n  ProxyJump ops@gw.example.com:2222\nHost b\n  HostName b\n  ProxyJump gw.example.com:2222\nHost c\n  HostName c\n  ProxyJump alice@gw.example.com:2222\n",
            std::slice::from_ref(&known),
        );
        let plan = plan_import(&rows, &rows, std::slice::from_ref(&known), None);
        assert_eq!(by_name(&plan, "a").jump, std::slice::from_ref(&known.id));
        assert_eq!(by_name(&plan, "b").jump, std::slice::from_ref(&known.id), "no user in the spec matches any user");
        let c_jump = by_name(&plan, "c").jump[0].clone();
        assert_ne!(c_jump, known.id, "a different user is a different jump host");
        assert_eq!(plan.hosts.len(), 4, "a, b, c and one new minimal jump host for alice@");
    }

    #[test]
    fn the_same_spec_twice_creates_one_jump_host() {
        let rows = rows(
            "Host a\n  HostName a\n  ProxyJump jump.example.com\nHost b\n  HostName b\n  ProxyJump jump.example.com\n",
            &[],
        );
        let plan = plan_import(&rows, &rows, &[], None);
        assert_eq!(plan.hosts.len(), 3);
        assert_eq!(by_name(&plan, "a").jump, by_name(&plan, "b").jump);
        assert_eq!(plan.jump_hosts, 1);
    }

    #[test]
    fn jump_cycles_are_cut() {
        let rows = rows(
            "Host a\n  HostName a\n  ProxyJump b\nHost b\n  HostName b\n  ProxyJump a\nHost s\n  HostName s\n  ProxyJump s\n",
            &[],
        );
        let plan = plan_import(&rows, &rows, &[], None);
        let (a, b, s) = (by_name(&plan, "a"), by_name(&plan, "b"), by_name(&plan, "s"));
        assert_eq!(a.jump, std::slice::from_ref(&b.id));
        assert!(b.jump.is_empty(), "the link back to a would close the cycle");
        assert!(s.jump.is_empty(), "a host cannot jump through itself");
    }

    #[test]
    fn jump_spec_parsing() {
        assert_eq!(parse_jump_spec("host"), (None, "host", None));
        assert_eq!(parse_jump_spec("host:2200"), (None, "host", Some(2200)));
        assert_eq!(parse_jump_spec("me@host"), (Some("me"), "host", None));
        assert_eq!(parse_jump_spec("me@host:22"), (Some("me"), "host", Some(22)));
        assert_eq!(parse_jump_spec("[::1]:2200"), (None, "::1", Some(2200)));
        assert_eq!(parse_jump_spec("me@[fe80::1]"), (Some("me"), "fe80::1", None));
        assert_eq!(parse_jump_spec("fe80::1"), (None, "fe80::1", None), "bare IPv6 has no port");
        assert_eq!(parse_jump_spec("host:notaport"), (None, "host:notaport", None));
    }
}
