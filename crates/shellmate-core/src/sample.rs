//! Sample host book taken from the prototype (`docs/Shellmate 原型.html`).
//!
//! Used until persistent storage lands in P2.

use crate::host::{AuthMethod, FingerprintState, Group, GroupTone, Host, HostBook, Protocol};

struct Spec {
    id: &'static str,
    name: &'static str,
    addr: &'static str,
    user: Option<&'static str>,
    port: Option<u16>,
    group: &'static str,
    tags: &'static [&'static str],
    fav: bool,
    recent: Option<u32>,
    proto: Protocol,
    auth: Option<AuthMethod>,
    jump: Option<&'static str>,
    baud: Option<u32>,
    fp: FingerprintState,
}

const BASE: Spec = Spec {
    id: "",
    name: "",
    addr: "",
    user: None,
    port: Some(22),
    group: "",
    tags: &[],
    fav: false,
    recent: None,
    proto: Protocol::Ssh,
    auth: Some(AuthMethod::PrivateKey),
    jump: None,
    baud: None,
    fp: FingerprintState::Known,
};

pub fn sample_book() -> HostBook {
    use AuthMethod::*;
    let specs = [
        Spec { id: "h1", name: "prod-web-01", addr: "10.0.1.21", user: Some("deploy"), group: "生产环境", tags: &["nginx", "k8s"], fav: true, recent: Some(2), ..BASE },
        Spec { id: "h2", name: "prod-web-02", addr: "10.0.1.22", user: Some("deploy"), group: "生产环境", tags: &["nginx", "k8s"], recent: Some(1440), ..BASE },
        Spec { id: "h3", name: "prod-db-01", addr: "10.0.2.8", user: Some("dba"), group: "生产环境", tags: &["mysql"], fav: true, recent: Some(15), auth: Some(Agent), jump: Some("bastion → jump-sh"), ..BASE },
        Spec { id: "h4", name: "bastion", addr: "bastion.corp.io", user: Some("ops"), port: Some(2222), group: "生产环境", tags: &["跳板"], auth: Some(KeyboardInteractive), ..BASE },
        Spec { id: "h5", name: "staging-api", addr: "172.16.4.10", user: Some("ubuntu"), group: "测试环境", tags: &["api"], recent: Some(4320), auth: Some(Password), fp: FingerprintState::Changed, ..BASE },
        Spec { id: "h6", name: "staging-redis", addr: "172.16.4.11", user: Some("ubuntu"), group: "测试环境", tags: &["redis"], ..BASE },
        Spec { id: "h7", name: "vps-tokyo", addr: "139.180.201.7", user: Some("root"), group: "个人", tags: &["blog"], fav: true, recent: Some(60), ..BASE },
        Spec { id: "h8", name: "dev-box", addr: "dev.home.arpa", user: Some("me"), group: "个人", tags: &["dev"], recent: Some(300), proto: Protocol::Mosh, auth: Some(Agent), ..BASE },
        Spec { id: "h9", name: "homelab-nas", addr: "192.168.31.50", user: Some("admin"), group: "个人", tags: &["nas"], auth: Some(Password), fp: FingerprintState::New, ..BASE },
        Spec { id: "h10", name: "ESP32-DevKit", addr: "/dev/cu.usbserial-0001", port: None, group: "设备", tags: &["iot"], recent: Some(30), proto: Protocol::Serial, auth: None, baud: Some(115_200), ..BASE },
        Spec { id: "h11", name: "office-router", addr: "192.168.1.1", user: Some("admin"), port: Some(23), group: "设备", tags: &["iot"], proto: Protocol::Telnet, auth: None, ..BASE },
    ];

    HostBook {
        groups: vec![
            Group { name: "生产环境".into(), tone: GroupTone::Accent },
            Group { name: "测试环境".into(), tone: GroupTone::Warn },
            Group { name: "个人".into(), tone: GroupTone::Ok },
            Group { name: "设备".into(), tone: GroupTone::Neutral },
        ],
        hosts: specs
            .into_iter()
            .map(|s| Host {
                id: s.id.into(),
                name: s.name.into(),
                addr: s.addr.into(),
                user: s.user.map(Into::into),
                port: s.port,
                group: s.group.into(),
                tags: s.tags.iter().map(|t| t.to_string()).collect(),
                favorite: s.fav,
                last_connected_mins: s.recent,
                protocol: s.proto,
                auth: s.auth,
                jump: s.jump.map(Into::into),
                baud: s.baud,
                fingerprint: s.fp,
            })
            .collect(),
    }
}
