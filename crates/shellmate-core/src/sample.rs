//! Sample host book taken from the prototype (`docs/Shellmate 原型.html`).
//!
//! Only for tests (this module is compiled for unit tests and the `test-support`
//! feature); the app starts with an empty host book.

use crate::host::{AuthMethod, Group, GroupId, GroupTone, Host, HostBook, HostId, Protocol};

/// "Now" of the sample data: `last_connected_at` values are offsets from it.
pub const SAMPLE_NOW: i64 = 1_800_000_000;

struct Spec {
    id: &'static str,
    name: &'static str,
    addr: &'static str,
    user: Option<&'static str>,
    port: u16,
    group: &'static str,
    tags: &'static [&'static str],
    fav: bool,
    recent_mins: Option<i64>,
    proto: Protocol,
    auth: AuthMethod,
    jump: &'static [&'static str],
}

const BASE: Spec = Spec {
    id: "",
    name: "",
    addr: "",
    user: None,
    port: 22,
    group: "",
    tags: &[],
    fav: false,
    recent_mins: None,
    proto: Protocol::Ssh,
    auth: AuthMethod::PrivateKey,
    jump: &[],
};

/// The prototype's 11 hosts in 4 groups. Ids are `h1`..`h11` and `g-prod`, `g-test`, `g-personal`, `g-devices`.
pub fn sample_book() -> HostBook {
    use AuthMethod::*;
    #[rustfmt::skip]
    let specs = [
        Spec { id: "h1", name: "prod-web-01", addr: "10.0.1.21", user: Some("deploy"), group: "g-prod", tags: &["nginx", "k8s"], fav: true, recent_mins: Some(2), ..BASE },
        Spec { id: "h2", name: "prod-web-02", addr: "10.0.1.22", user: Some("deploy"), group: "g-prod", tags: &["nginx", "k8s"], recent_mins: Some(1440), ..BASE },
        Spec { id: "h3", name: "prod-db-01", addr: "10.0.2.8", user: Some("dba"), group: "g-prod", tags: &["mysql"], fav: true, recent_mins: Some(15), auth: Agent, jump: &["h4"], ..BASE },
        Spec { id: "h4", name: "bastion", addr: "bastion.corp.io", user: Some("ops"), port: 2222, group: "g-prod", tags: &["跳板"], auth: KeyboardInteractive, ..BASE },
        Spec { id: "h5", name: "staging-api", addr: "172.16.4.10", user: Some("ubuntu"), group: "g-test", tags: &["api"], recent_mins: Some(4320), auth: Password, ..BASE },
        Spec { id: "h6", name: "staging-redis", addr: "172.16.4.11", user: Some("ubuntu"), group: "g-test", tags: &["redis"], ..BASE },
        Spec { id: "h7", name: "vps-tokyo", addr: "139.180.201.7", user: Some("root"), group: "g-personal", tags: &["blog"], fav: true, recent_mins: Some(60), ..BASE },
        Spec { id: "h8", name: "dev-box", addr: "dev.home.arpa", user: Some("me"), group: "g-personal", tags: &["dev"], recent_mins: Some(300), proto: Protocol::Mosh, auth: Agent, ..BASE },
        Spec { id: "h9", name: "homelab-nas", addr: "192.168.31.50", user: Some("admin"), group: "g-personal", tags: &["nas"], auth: Password, ..BASE },
        Spec { id: "h10", name: "ESP32-DevKit", addr: "/dev/cu.usbserial-0001", port: 0, group: "g-devices", tags: &["iot"], recent_mins: Some(30), proto: Protocol::Serial, ..BASE },
        Spec { id: "h11", name: "office-router", addr: "192.168.1.1", user: Some("admin"), port: 23, group: "g-devices", tags: &["iot"], proto: Protocol::Telnet, ..BASE },
    ];

    let groups = [
        ("g-prod", "生产环境", GroupTone::Accent),
        ("g-test", "测试环境", GroupTone::Warn),
        ("g-personal", "个人", GroupTone::Ok),
        ("g-devices", "设备", GroupTone::Neutral),
    ]
    .into_iter()
    .enumerate()
    .map(|(sort, (id, name, tone))| Group { id: GroupId::from_raw(id), name: name.into(), tone, sort: sort as i64 })
    .collect();

    let hosts = specs
        .into_iter()
        .map(|s| Host {
            id: HostId::from_raw(s.id),
            name: s.name.into(),
            addr: s.addr.into(),
            port: s.port,
            user: s.user.map(Into::into),
            group: Some(GroupId::from_raw(s.group)),
            tags: s.tags.iter().map(|t| t.to_string()).collect(),
            favorite: s.fav,
            protocol: s.proto,
            auth: s.auth,
            identity_file: None,
            jump: s.jump.iter().map(|j| HostId::from_raw(*j)).collect(),
            last_connected_at: s.recent_mins.map(|m| SAMPLE_NOW - m * 60),
        })
        .collect();

    HostBook::new(groups, hosts)
}
