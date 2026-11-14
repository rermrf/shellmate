//! Addresses as users write them: `3306`, `127.0.0.1:3306`, `[::1]:3306`, `db.internal:5432`.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::rule::ForwardKind;

/// A host and a port.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    /// IP address or host name (IPv6 without brackets).
    pub host: String,
    /// TCP port.
    pub port: u16,
}

impl Endpoint {
    /// An endpoint from parts.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self { host: host.into(), port }
    }

    /// The same host with another port.
    pub fn with_port(&self, port: u16) -> Self {
        Self { host: self.host.clone(), port }
    }

    /// Whether the host is an address of this machine's loopback interface.
    pub fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost") || self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }
}

impl fmt::Display for Endpoint {
    /// `host:port`, `[::1]:port` for IPv6.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// Why an address was not accepted. `Display` is the message shown next to the input.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AddrError {
    /// Nothing was entered.
    #[error("地址不能为空")]
    Empty,
    /// No port was given.
    #[error("缺少端口，请写成「主机:端口」")]
    MissingPort,
    /// A target needs a host, only a port was given.
    #[error("缺少主机，请写成「主机:端口」")]
    MissingHost,
    /// The port is not a number.
    #[error("端口「{0}」无效，需要是 1–65535 之间的数字")]
    BadPort(String),
    /// The port is a number outside the allowed range.
    #[error("端口 {0} 超出范围，需要在 1–65535 之间")]
    PortRange(u32),
    /// Port 0 is only meaningful for remote forwards.
    #[error("本地监听端口不能为 0，需要在 1–65535 之间（只有远程转发可以填 0，表示由服务器分配）")]
    ZeroPort,
    /// Port 0 as the target of a connection.
    #[error("目标端口不能为 0，需要在 1–65535 之间")]
    ZeroTargetPort,
    /// An IPv6 address without brackets.
    #[error("IPv6 地址需要用方括号括起来，例如 [::1]:3306")]
    BareIpv6,
    /// `[` without `]`.
    #[error("IPv6 地址缺少右方括号 ]")]
    UnclosedBracket,
    /// Something follows the closing bracket other than `:port`.
    #[error("方括号后面应该是「:端口」")]
    TrailingAfterBracket,
    /// Brackets around something that is not an IPv6 address.
    #[error("方括号里需要是 IPv6 地址，例如 [::1]")]
    BadIpv6(String),
    /// The host contains characters a host name cannot have.
    #[error("主机「{0}」无效，只能包含字母、数字、点、横线和下划线")]
    BadHost(String),
    /// The host looks like an IPv4 address but is not one.
    #[error("IP 地址「{0}」无效")]
    BadIpv4(String),
    /// A local listen address has to be an address of this machine, not a name.
    #[error("本地监听地址需要是 IP 地址（127.0.0.1、0.0.0.0、[::1]），不能写主机名「{0}」")]
    ListenNotIp(String),
}

/// Parses the address a rule listens on. A bare port means `127.0.0.1:port`. Local and dynamic
/// forwards listen on this machine, so the host must be an IP address (`localhost` counts as
/// `127.0.0.1`) and the port 1–65535; a remote forward's address is passed to the server as
/// written and its port may be 0 (the server picks one).
pub fn parse_listen(text: &str, kind: ForwardKind) -> Result<Endpoint, AddrError> {
    let (host, port) = split(text)?;
    let remote = kind == ForwardKind::Remote;
    let port = match (port, remote) {
        (0, true) => 0,
        (0, false) => return Err(AddrError::ZeroPort),
        (port, _) => u16::try_from(port).map_err(|_| AddrError::PortRange(port))?,
    };
    let host = match host {
        None => "127.0.0.1".to_string(),
        Some(host) if remote => host,
        Some(host) if host.eq_ignore_ascii_case("localhost") => "127.0.0.1".to_string(),
        Some(host) if host.parse::<IpAddr>().is_ok() => host,
        Some(host) => return Err(AddrError::ListenNotIp(host)),
    };
    Ok(Endpoint { host, port })
}

/// Parses a target: `host:port` or `[v6]:port`, port 1–65535.
pub fn parse_target(text: &str) -> Result<Endpoint, AddrError> {
    let (host, port) = split(text)?;
    let host = host.ok_or(AddrError::MissingHost)?;
    let port = match port {
        0 => return Err(AddrError::ZeroTargetPort),
        port => u16::try_from(port).map_err(|_| AddrError::PortRange(port))?,
    };
    Ok(Endpoint { host, port })
}

/// Splits `[host:]port` and checks the host; the port is returned unchecked beyond being a
/// number up to 99999 (the caller knows whether 0 is fine).
fn split(text: &str) -> Result<(Option<String>, u32), AddrError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AddrError::Empty);
    }
    if let Some(rest) = text.strip_prefix('[') {
        let (inside, after) = rest.split_once(']').ok_or(AddrError::UnclosedBracket)?;
        inside.parse::<Ipv6Addr>().map_err(|_| AddrError::BadIpv6(inside.to_string()))?;
        let port = after.strip_prefix(':').ok_or(if after.is_empty() {
            AddrError::MissingPort
        } else {
            AddrError::TrailingAfterBracket
        })?;
        return Ok((Some(inside.to_string()), parse_port(port)?));
    }
    match text.matches(':').count() {
        0 if text.bytes().all(|b| b.is_ascii_digit()) => Ok((None, parse_port(text)?)),
        0 => {
            check_host(text)?;
            Err(AddrError::MissingPort)
        }
        1 => {
            let (host, port) = text.split_once(':').expect("one colon");
            if host.is_empty() {
                return Err(AddrError::MissingHost);
            }
            check_host(host)?;
            Ok((Some(host.to_string()), parse_port(port)?))
        }
        _ => Err(AddrError::BareIpv6),
    }
}

fn parse_port(text: &str) -> Result<u32, AddrError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AddrError::MissingPort);
    }
    if text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AddrError::BadPort(text.to_string()));
    }
    text.parse().map_err(|_| AddrError::BadPort(text.to_string()))
}

fn check_host(host: &str) -> Result<(), AddrError> {
    let name_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '.' | '_');
    if host.len() > 253 || !host.chars().all(name_char) {
        return Err(AddrError::BadHost(host.to_string()));
    }
    // Digits and dots only: meant as an IPv4 address.
    if host.bytes().all(|b| b.is_ascii_digit() || b == b'.') && host.parse::<Ipv4Addr>().is_err() {
        return Err(AddrError::BadIpv4(host.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ForwardKind::{Dynamic, Local, Remote};

    fn listen(text: &str, kind: ForwardKind) -> Endpoint {
        parse_listen(text, kind).unwrap_or_else(|err| panic!("{text:?}: {err}"))
    }

    #[test]
    fn listen_addresses_in_every_spelling() {
        assert_eq!(listen("3306", Local), Endpoint::new("127.0.0.1", 3306));
        assert_eq!(listen(" 3306 ", Local), Endpoint::new("127.0.0.1", 3306));
        assert_eq!(listen("127.0.0.1:3306", Local), Endpoint::new("127.0.0.1", 3306));
        assert_eq!(listen("[::1]:3306", Local), Endpoint::new("::1", 3306));
        assert_eq!(listen("0.0.0.0:3306", Local), Endpoint::new("0.0.0.0", 3306));
        assert_eq!(listen("[::]:1080", Dynamic), Endpoint::new("::", 1080));
        assert_eq!(listen("localhost:8080", Local), Endpoint::new("127.0.0.1", 8080));
        assert_eq!(listen("65535", Dynamic).port, 65535);
        assert_eq!(listen("1", Local).port, 1);
    }

    #[test]
    fn remote_listen_addresses_are_passed_on_and_may_use_port_zero() {
        assert_eq!(listen("9000", Remote), Endpoint::new("127.0.0.1", 9000));
        assert_eq!(listen("0", Remote), Endpoint::new("127.0.0.1", 0));
        assert_eq!(listen("0.0.0.0:0", Remote), Endpoint::new("0.0.0.0", 0));
        assert_eq!(listen("localhost:9000", Remote), Endpoint::new("localhost", 9000));
        assert_eq!(listen("[::1]:9000", Remote), Endpoint::new("::1", 9000));
        assert_eq!(listen("gw.internal:9000", Remote), Endpoint::new("gw.internal", 9000));
    }

    #[test]
    fn listen_errors_are_explained_in_chinese() {
        let err = |text: &str, kind| parse_listen(text, kind).unwrap_err();
        assert_eq!(err("", Local), AddrError::Empty);
        assert_eq!(err("   ", Local), AddrError::Empty);
        assert_eq!(err("0", Local), AddrError::ZeroPort);
        assert_eq!(err("127.0.0.1:0", Dynamic), AddrError::ZeroPort);
        assert_eq!(err("65536", Local), AddrError::PortRange(65536));
        assert_eq!(err("99999", Remote), AddrError::PortRange(99999));
        assert_eq!(err("123456", Local), AddrError::BadPort("123456".into()));
        assert_eq!(err("-1", Local), AddrError::MissingPort);
        assert_eq!(err("abc", Local), AddrError::MissingPort);
        assert_eq!(err("127.0.0.1", Local), AddrError::MissingPort);
        assert_eq!(err("127.0.0.1:", Local), AddrError::MissingPort);
        assert_eq!(err("127.0.0.1:abc", Local), AddrError::BadPort("abc".into()));
        assert_eq!(err("db.internal:5432", Local), AddrError::ListenNotIp("db.internal".into()));
        assert_eq!(err("::1:3306", Local), AddrError::BareIpv6);
        assert_eq!(err("[::1:3306", Local), AddrError::UnclosedBracket);
        assert_eq!(err("[::1]", Local), AddrError::MissingPort);
        assert_eq!(err("[::1]3306", Local), AddrError::TrailingAfterBracket);
        assert_eq!(err("[abc]:80", Local), AddrError::BadIpv6("abc".into()));
        assert_eq!(err("300.1.1.1:80", Local), AddrError::BadIpv4("300.1.1.1".into()));
        assert_eq!(err("a b:80", Remote), AddrError::BadHost("a b".into()));
        assert_eq!(err(":80", Local), AddrError::MissingHost);

        for text in ["", "0", "65536", "::1:80", "[::1", "db.internal:5432"] {
            let shown = parse_listen(text, Local).unwrap_err().to_string();
            assert!(shown.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)), "{shown}");
        }
    }

    #[test]
    fn targets() {
        assert_eq!(parse_target("db.internal:5432"), Ok(Endpoint::new("db.internal", 5432)));
        assert_eq!(parse_target("localhost:3000"), Ok(Endpoint::new("localhost", 3000)));
        assert_eq!(parse_target("10.0.0.5:22"), Ok(Endpoint::new("10.0.0.5", 22)));
        assert_eq!(parse_target("[fe80::1]:22"), Ok(Endpoint::new("fe80::1", 22)));
        assert_eq!(parse_target("  my-db_1.local:1  "), Ok(Endpoint::new("my-db_1.local", 1)));
        assert_eq!(parse_target("5432"), Err(AddrError::MissingHost));
        assert_eq!(parse_target(""), Err(AddrError::Empty));
        assert_eq!(parse_target("db.internal"), Err(AddrError::MissingPort));
        assert_eq!(parse_target("db.internal:0"), Err(AddrError::ZeroTargetPort));
        assert_eq!(parse_target("db.internal:70000"), Err(AddrError::PortRange(70000)));
        assert_eq!(parse_target("fe80::1:22"), Err(AddrError::BareIpv6));
        assert_eq!(parse_target("db/x:80"), Err(AddrError::BadHost("db/x".into())));
        assert_eq!(parse_target("1.2.3:80"), Err(AddrError::BadIpv4("1.2.3".into())));
    }

    #[test]
    fn endpoints_print_the_way_they_are_written() {
        assert_eq!(Endpoint::new("127.0.0.1", 80).to_string(), "127.0.0.1:80");
        assert_eq!(Endpoint::new("::1", 80).to_string(), "[::1]:80");
        assert_eq!(Endpoint::new("db.internal", 5432).to_string(), "db.internal:5432");
        for text in ["[::1]:80", "10.0.0.1:81", "host.name:82"] {
            assert_eq!(parse_target(text).unwrap().to_string(), text);
        }
    }

    #[test]
    fn loopback_detection() {
        assert!(Endpoint::new("127.0.0.1", 1).is_loopback());
        assert!(Endpoint::new("::1", 1).is_loopback());
        assert!(Endpoint::new("localhost", 1).is_loopback());
        assert!(!Endpoint::new("0.0.0.0", 1).is_loopback());
        assert!(!Endpoint::new("192.168.1.5", 1).is_loopback());
        assert!(!Endpoint::new("::", 1).is_loopback());
    }
}
