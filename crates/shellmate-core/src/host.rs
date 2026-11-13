//! Host book domain model: saved hosts and the groups they belong to.

/// Connection protocol of a saved host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Ssh,
    Mosh,
    Telnet,
    Serial,
}

impl Protocol {
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMethod {
    PrivateKey,
    Password,
    Agent,
    KeyboardInteractive,
}

impl AuthMethod {
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
    #[default]
    Known,
    New,
    Changed,
}

/// Color tone used to tint a group's host avatars.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GroupTone {
    Accent,
    Warn,
    Ok,
    #[default]
    Neutral,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub tone: GroupTone,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Host {
    pub id: String,
    pub name: String,
    /// Hostname, IP address, or serial device path.
    pub addr: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub group: String,
    pub tags: Vec<String>,
    pub favorite: bool,
    /// Minutes since the last connection, if ever connected.
    pub last_connected_mins: Option<u32>,
    pub protocol: Protocol,
    pub auth: Option<AuthMethod>,
    /// ProxyJump chain, e.g. `bastion → jump-sh`.
    pub jump: Option<String>,
    pub baud: Option<u32>,
    pub fingerprint: FingerprintState,
}

impl Host {
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

    /// Second line under the host name: `user@addr[:port]`, or `device · baud` for serial.
    pub fn address_line(&self) -> String {
        if self.protocol == Protocol::Serial {
            return format!("{} · {}", self.addr, self.baud.unwrap_or(115_200));
        }
        let mut line = String::new();
        if let Some(user) = &self.user {
            line.push_str(user);
            line.push('@');
        }
        line.push_str(&self.addr);
        if let Some(port) = self.port.filter(|p| *p != 22) {
            line.push_str(&format!(":{port}"));
        }
        line
    }

    /// Small badge shown on the right of a host row.
    pub fn badge(&self) -> Option<&'static str> {
        match self.protocol {
            Protocol::Ssh => self.jump.as_ref().map(|_| "ProxyJump"),
            other => Some(other.label()),
        }
    }

    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let haystack = format!("{} {} {}", self.name, self.addr, self.tags.join(" ")).to_lowercase();
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

/// All saved hosts plus group ordering.
#[derive(Clone, Debug, Default)]
pub struct HostBook {
    pub groups: Vec<Group>,
    pub hosts: Vec<Host>,
}

impl HostBook {
    pub fn host(&self, id: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.id == id)
    }

    pub fn tone_of(&self, group: &str) -> GroupTone {
        self.groups
            .iter()
            .find(|g| g.name == group)
            .map(|g| g.tone)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> Host {
        Host {
            id: name.into(),
            name: name.into(),
            addr: "10.0.0.1".into(),
            user: Some("root".into()),
            port: Some(22),
            group: "g".into(),
            tags: vec!["nginx".into()],
            favorite: false,
            last_connected_mins: None,
            protocol: Protocol::Ssh,
            auth: Some(AuthMethod::PrivateKey),
            jump: None,
            baud: None,
            fingerprint: FingerprintState::Known,
        }
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
        h.port = Some(2222);
        assert_eq!(h.address_line(), "root@10.0.0.1:2222");
        h.user = None;
        assert_eq!(h.address_line(), "10.0.0.1:2222");
    }

    #[test]
    fn address_line_for_serial_shows_baud() {
        let mut h = host("esp");
        h.protocol = Protocol::Serial;
        h.addr = "/dev/cu.usbserial-0001".into();
        h.baud = Some(115_200);
        assert_eq!(h.address_line(), "/dev/cu.usbserial-0001 · 115200");
    }

    #[test]
    fn badge_shows_protocol_or_proxyjump() {
        let mut h = host("a");
        assert_eq!(h.badge(), None);
        h.jump = Some("bastion".into());
        assert_eq!(h.badge(), Some("ProxyJump"));
        h.protocol = Protocol::Mosh;
        assert_eq!(h.badge(), Some("Mosh"));
    }

    #[test]
    fn matches_name_addr_and_tags_case_insensitively() {
        let h = host("Prod-Web");
        assert!(h.matches(""));
        assert!(h.matches("  prod "));
        assert!(h.matches("10.0.0"));
        assert!(h.matches("NGINX"));
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
}
