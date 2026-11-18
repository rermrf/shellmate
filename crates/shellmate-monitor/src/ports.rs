//! Local listening sockets. Keep the sampled identity separate from the newer process snapshot.
use crate::{Process, Snapshot};
use std::net::IpAddr;
use std::sync::Arc;

/// Transport protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Protocol {
    /// Listening stream socket.
    Tcp,
    /// Bound datagram socket.
    Udp,
}

/// Address families served by a listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    /// IPv4.
    V4,
    /// IPv6.
    V6,
    /// IPv4 and IPv6.
    Both,
}

/// Where a socket accepts traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exposure {
    /// A wildcard bind.
    All,
    /// Only loopback interfaces.
    Local,
    /// One or more specific addresses.
    Bound,
}

/// The scope of one local address (without its port).
pub fn exposure(address: &str) -> Exposure {
    let ip = address.split('%').next().and_then(|host| host.parse::<IpAddr>().ok());
    if address == "*" || ip.is_some_and(|ip| ip.is_unspecified()) {
        Exposure::All
    } else if ip.is_some_and(|ip| ip.is_loopback()) {
        Exposure::Local
    } else {
        Exposure::Bound
    }
}

/// One listener, possibly coalesced across address families.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listener {
    /// Transport.
    pub protocol: Protocol,
    /// IP families.
    pub family: Family,
    /// Sorted, distinct numeric addresses or `*`.
    pub addresses: Vec<String>,
    /// Bound port.
    pub port: u16,
    /// Owning PID.
    pub pid: u32,
    /// Process name, available without ps.
    pub name: String,
    /// Login reported by lsof, when available without a process snapshot.
    pub user: Option<String>,
    /// Process identity when known at sampling time; never act on a known recycled PID.
    pub owner_start: Option<u64>,
}

impl Listener {
    /// Current process details if the owner is still present.
    pub fn owner<'a>(&self, snapshot: Option<&'a Snapshot>) -> Option<&'a Process> {
        snapshot?
            .process(self.pid)
            .filter(|proc| self.owner_start.is_none_or(|start| start == proc.start_time))
    }

    /// A usable host:port, including brackets around an IPv6 literal.
    pub fn endpoint(&self) -> String {
        let host = self.addresses.iter().find(|addr| exposure(addr) == Exposure::Bound);
        match host {
            Some(host) if host.contains(':') => format!("[{host}]:{}", self.port),
            Some(host) => format!("{host}:{}", self.port),
            None => format!("localhost:{}", self.port),
        }
    }

    /// HTTP target for the browser action, never offered for datagrams.
    pub fn browser_url(&self) -> Option<String> {
        (self.protocol == Protocol::Tcp).then(|| format!("http://{}", self.endpoint().replace('%', "%25")))
    }
}

/// Source of a protocol's last successful socket enumeration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PortSource {
    /// System socket table, preferred when it contains listeners.
    #[default]
    Netstat,
    /// Per-process file descriptors visible to the current user.
    Lsof,
}

/// TCP and UDP can independently require the fallback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortSources {
    /// Source for listening stream sockets.
    pub tcp: PortSource,
    /// Source for bound datagram sockets.
    pub udp: PortSource,
}

/// Last complete sample, retained across failures and while the ports page is closed.
#[derive(Clone, Debug, Default)]
pub struct PortList {
    /// `None` until the first successful read (distinct from zero listeners).
    pub rows: Option<Arc<Vec<Listener>>>,
    /// Sources used to collect these rows.
    pub sources: PortSources,
    /// The last refresh failed; cleared on recovery.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_and_browser_targets_follow_the_bind() {
        for (addr, scope, endpoint, url) in [
            ("*", Exposure::All, "localhost:80", "http://localhost:80"),
            ("0.0.0.0", Exposure::All, "localhost:80", "http://localhost:80"),
            ("::", Exposure::All, "localhost:80", "http://localhost:80"),
            ("::1", Exposure::Local, "localhost:80", "http://localhost:80"),
            ("127.42.3.9", Exposure::Local, "localhost:80", "http://localhost:80"),
            ("192.0.2.11", Exposure::Bound, "192.0.2.11:80", "http://192.0.2.11:80"),
            (
                "2001:db8::1",
                Exposure::Bound,
                "[2001:db8::1]:80",
                "http://[2001:db8::1]:80",
            ),
            (
                "fe80::1%en0",
                Exposure::Bound,
                "[fe80::1%en0]:80",
                "http://[fe80::1%25en0]:80",
            ),
        ] {
            let mut row = Listener {
                protocol: Protocol::Tcp,
                family: Family::Both,
                addresses: vec![addr.into()],
                port: 80,
                pid: 55,
                name: "daemon".into(),
                user: None,
                owner_start: None,
            };
            assert_eq!(exposure(addr), scope);
            assert_eq!(row.endpoint(), endpoint);
            assert_eq!(row.browser_url().as_deref(), Some(url));
            row.protocol = Protocol::Udp;
            assert!(row.browser_url().is_none());
        }
    }
}
