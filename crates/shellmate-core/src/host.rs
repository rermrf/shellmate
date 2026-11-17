//! Host book domain model: saved hosts and the groups they belong to.

use std::fmt;
use std::path::PathBuf;

/// Defines a stable identifier backed by a UUID v4 string.
macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// A fresh random id (UUID v4).
            pub fn generate() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }

            /// Wraps an id read back from storage (or a test fixture) without validating it.
            pub fn from_raw(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            /// The id as text, e.g. for database columns and UI element ids.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(
    /// Stable identifier of a saved host. It never changes, even when the host is renamed.
    HostId
);
id_type!(
    /// Stable identifier of a host group.
    GroupId
);

/// Connection protocol of a saved host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Protocol {
    /// Secure Shell.
    #[default]
    Ssh,
    /// SSH bootstrap with a Mosh UDP terminal.
    Mosh,
    /// Telnet over TCP (plaintext).
    Telnet,
    /// Serial console.
    Serial,
}

impl Protocol {
    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Ssh => "SSH",
            Protocol::Mosh => "Mosh",
            Protocol::Telnet => "Telnet",
            Protocol::Serial => "串口",
        }
    }
}

/// How a host authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AuthMethod {
    /// A private key file (the configured one, or the default keys of `~/.ssh`).
    #[default]
    PrivateKey,
    /// A password.
    Password,
    /// Keys held by ssh-agent.
    Agent,
    /// Keyboard-interactive challenges (OTP, PAM prompts).
    KeyboardInteractive,
}

impl AuthMethod {
    /// Name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            AuthMethod::PrivateKey => "私钥",
            AuthMethod::Password => "密码",
            AuthMethod::Agent => "ssh-agent",
            AuthMethod::KeyboardInteractive => "键盘交互",
        }
    }
}

/// State of the host key compared with `known_hosts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FingerprintState {
    /// The recorded key matches.
    #[default]
    Known,
    /// The host was never seen.
    New,
    /// A different key than the recorded one.
    Changed,
}

/// Color tone used to tint a group's host avatars.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GroupTone {
    /// Accent color (blue).
    Accent,
    /// Warning color (amber).
    Warn,
    /// Success color (green).
    Ok,
    /// No tint.
    #[default]
    Neutral,
}

/// A named collection of hosts shown as one sidebar section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// Stable id.
    pub id: GroupId,
    /// Display name; unique among groups.
    pub name: String,
    /// Avatar tint of the group's hosts.
    pub tone: GroupTone,
    /// Position among the groups (ascending).
    pub sort: i64,
}

impl Group {
    /// A new group with a fresh id, sorted first.
    pub fn new(name: impl Into<String>, tone: GroupTone) -> Self {
        Self { id: GroupId::generate(), name: name.into(), tone, sort: 0 }
    }
}

/// A saved connection target. Contains no secrets: passwords and key
/// passphrases live in the keychain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    /// Stable id.
    pub id: HostId,
    /// Display name.
    pub name: String,
    /// Hostname or IP address.
    pub addr: String,
    /// TCP port (22 by default).
    pub port: u16,
    /// Login name; `None` means the local user name.
    pub user: Option<String>,
    /// The group the host belongs to, if any.
    pub group: Option<GroupId>,
    /// Legacy free-form tags retained for storage and bundle compatibility.
    pub tags: Vec<String>,
    /// Shown in the favorites tab.
    pub favorite: bool,
    /// Connection protocol.
    pub protocol: Protocol,
    /// Authentication method.
    pub auth: AuthMethod,
    /// Serial framing (retained when changing protocols).
    pub serial: crate::SerialSettings,
    /// Private key file for [`AuthMethod::PrivateKey`]; `None` uses the default keys.
    pub identity_file: Option<PathBuf>,
    /// ProxyJump chain, first hop first. Each jump host may have jump hosts of its own.
    pub jump: Vec<HostId>,
    /// Unix time (seconds) of the last connection.
    pub last_connected_at: Option<i64>,
}

impl Host {
    /// A new SSH host with a fresh id and defaults: port 22, key authentication.
    pub fn new(name: impl Into<String>, addr: impl Into<String>) -> Self {
        Self {
            id: HostId::generate(),
            name: name.into(),
            addr: addr.into(),
            port: 22,
            user: None,
            group: None,
            tags: Vec::new(),
            favorite: false,
            protocol: Protocol::Ssh,
            auth: AuthMethod::default(),
            serial: Default::default(),
            identity_file: None,
            jump: Vec::new(),
            last_connected_at: None,
        }
    }

    /// Two-letter avatar text: initials of the first two dash-separated words
    /// that contain a letter (`prod-web-01` → `PW`, `bastion` → `B`).
    pub fn monogram(&self) -> String {
        self.name
            .split('-')
            .filter(|w| w.chars().any(|c| c.is_alphabetic()))
            .take(2)
            .filter_map(|w| w.chars().next())
            .flat_map(char::to_uppercase)
            .collect()
    }

    /// Second line under the host name: `user@addr[:port]` (just the device path for serial).
    pub fn address_line(&self) -> String {
        if self.protocol == Protocol::Serial {
            return self.addr.clone();
        }
        let mut line = String::new();
        if let Some(user) = &self.user {
            line.push_str(user);
            line.push('@');
        }
        line.push_str(&self.addr);
        if self.port != 22 {
            line.push_str(&format!(":{}", self.port));
        }
        line
    }

    /// Small badge shown on the right of a host row.
    pub fn badge(&self) -> Option<&'static str> {
        match self.protocol {
            Protocol::Ssh => (!self.jump.is_empty()).then_some("ProxyJump"),
            other => Some(other.label()),
        }
    }

    /// Whether the host matches a search query (name or address, case-insensitive).
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let haystack = format!("{} {}", self.name, self.addr).to_lowercase();
        haystack.contains(&query)
    }
}

/// Human readable "time ago" in the prototype's wording.
pub fn relative_time(mins: u32) -> String {
    match mins {
        0 => "刚刚".to_string(),
        1..=59 => format!("{mins} 分钟前"),
        60..=1439 => format!("{} 小时前", mins / 60),
        1440..=2879 => "昨天".to_string(),
        _ => format!("{} 天前", mins / 1440),
    }
}

/// Whole minutes from `then` to `now` (both unix seconds), never negative.
pub fn minutes_since(now: i64, then: i64) -> u32 {
    u32::try_from((now - then).max(0) / 60).unwrap_or(u32::MAX)
}

/// All saved hosts plus group ordering.
#[derive(Clone, Debug, Default)]
pub struct HostBook {
    /// Groups in display order.
    pub groups: Vec<Group>,
    /// Hosts in storage order.
    pub hosts: Vec<Host>,
}

impl HostBook {
    /// A book with groups sorted by their `sort` value, then name.
    pub fn new(mut groups: Vec<Group>, hosts: Vec<Host>) -> Self {
        groups.sort_by(|a, b| a.sort.cmp(&b.sort).then_with(|| a.name.cmp(&b.name)));
        Self { groups, hosts }
    }

    /// Looks a host up by id.
    pub fn host(&self, id: &HostId) -> Option<&Host> {
        self.hosts.iter().find(|h| &h.id == id)
    }

    /// Looks a group up by id.
    pub fn group(&self, id: &GroupId) -> Option<&Group> {
        self.groups.iter().find(|g| &g.id == id)
    }

    /// Tone of a group; neutral for no group or an unknown one.
    pub fn tone_of(&self, group: Option<&GroupId>) -> GroupTone {
        group.and_then(|id| self.group(id)).map(|g| g.tone).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> Host {
        let mut host = Host::new(name, "10.0.0.1");
        host.user = Some("root".into());
        host.tags = vec!["nginx".into()];
        host
    }

    #[test]
    fn ids_are_unique_uuids() {
        let (a, b) = (HostId::generate(), HostId::generate());
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 36);
        assert_eq!(a.as_str().matches('-').count(), 4);
        assert_eq!(a.as_str().as_bytes()[14], b'4', "version nibble of a v4 UUID");
        assert_eq!(HostId::from_raw(a.as_str()), a);
        assert_eq!(a.to_string(), a.as_str());
    }

    #[test]
    fn new_host_has_sensible_defaults() {
        let host = Host::new("web", "example.com");
        assert_eq!((host.port, host.protocol, host.auth), (22, Protocol::Ssh, AuthMethod::PrivateKey));
        assert!(
            host.user.is_none() && host.group.is_none() && host.jump.is_empty() && host.last_connected_at.is_none()
        );
    }

    #[test]
    fn monogram_uses_first_two_lettered_words() {
        assert_eq!(host("prod-web-01").monogram(), "PW");
        assert_eq!(host("bastion").monogram(), "B");
        assert_eq!(host("ESP32-DevKit").monogram(), "ED");
        assert_eq!(host("01-02").monogram(), "");
    }

    #[test]
    fn address_line_hides_default_port() {
        let mut h = host("a");
        assert_eq!(h.address_line(), "root@10.0.0.1");
        h.port = 2222;
        assert_eq!(h.address_line(), "root@10.0.0.1:2222");
        h.user = None;
        assert_eq!(h.address_line(), "10.0.0.1:2222");
    }

    #[test]
    fn address_line_for_serial_is_the_device() {
        let mut h = host("esp");
        h.protocol = Protocol::Serial;
        h.addr = "/dev/cu.usbserial-0001".into();
        h.port = 0;
        assert_eq!(h.address_line(), "/dev/cu.usbserial-0001");
    }

    #[test]
    fn badge_shows_protocol_or_proxyjump() {
        let mut h = host("a");
        assert_eq!(h.badge(), None);
        h.jump = vec![HostId::from_raw("bastion")];
        assert_eq!(h.badge(), Some("ProxyJump"));
        h.protocol = Protocol::Mosh;
        assert_eq!(h.badge(), Some("Mosh"));
    }

    #[test]
    fn matches_name_and_addr_case_insensitively_but_not_tags() {
        let h = host("Prod-Web");
        assert!(h.matches(""));
        assert!(h.matches("  prod "));
        assert!(h.matches("10.0.0"));
        assert!(!h.matches("NGINX"));
        assert!(!h.matches("mysql"));
    }

    #[test]
    fn relative_time_wording() {
        assert_eq!(relative_time(0), "刚刚");
        assert_eq!(relative_time(2), "2 分钟前");
        assert_eq!(relative_time(60), "1 小时前");
        assert_eq!(relative_time(300), "5 小时前");
        assert_eq!(relative_time(1440), "昨天");
        assert_eq!(relative_time(4320), "3 天前");
    }

    #[test]
    fn minutes_since_clamps_at_zero() {
        assert_eq!(minutes_since(1_000, 1_000 - 150), 2);
        assert_eq!(minutes_since(1_000, 1_000), 0);
        assert_eq!(minutes_since(1_000, 2_000), 0, "a timestamp from the future is not negative");
    }

    #[test]
    fn book_sorts_groups_and_resolves_tones() {
        let mut late = Group::new("b", GroupTone::Warn);
        late.sort = 2;
        let mut early = Group::new("z", GroupTone::Ok);
        early.sort = 1;
        let (late_id, early_id) = (late.id.clone(), early.id.clone());
        let book = HostBook::new(vec![late, early], vec![]);
        assert_eq!(book.groups[0].id, early_id);
        assert_eq!(book.tone_of(Some(&late_id)), GroupTone::Warn);
        assert_eq!(book.tone_of(None), GroupTone::Neutral);
        assert_eq!(book.tone_of(Some(&GroupId::from_raw("missing"))), GroupTone::Neutral);
    }
}
