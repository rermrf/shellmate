//! Taking a local port, and finding another one when it is taken.

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, TcpListener, ToSocketAddrs};

use crate::addr::Endpoint;
use crate::state::RuleError;

/// Listens on `listen`, ready to be handed to tokio. A taken port gives
/// [`RuleError::PortInUse`] with a free port to offer instead.
pub(crate) fn bind(listen: &Endpoint) -> Result<TcpListener, RuleError> {
    let addr = resolve(listen).map_err(|reason| bind_failed(listen, reason))?;
    let listener = TcpListener::bind(addr).map_err(|err| match err.kind() {
        ErrorKind::AddrInUse => RuleError::PortInUse {
            host: listen.host.clone(),
            port: listen.port,
            suggestion: suggest_port(addr.ip(), listen.port),
        },
        ErrorKind::PermissionDenied => {
            bind_failed(listen, "没有权限监听这个端口（1024 以下的端口需要管理员权限）".into())
        }
        ErrorKind::AddrNotAvailable => bind_failed(listen, "本机没有这个地址".into()),
        _ => bind_failed(listen, err.to_string()),
    })?;
    listener.set_nonblocking(true).map_err(|err| bind_failed(listen, err.to_string()))?;
    Ok(listener)
}

fn bind_failed(listen: &Endpoint, reason: String) -> RuleError {
    RuleError::BindFailed { host: listen.host.clone(), port: listen.port, reason }
}

fn resolve(listen: &Endpoint) -> Result<SocketAddr, String> {
    if let Ok(ip) = listen.host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, listen.port));
    }
    (listen.host.as_str(), listen.port)
        .to_socket_addrs()
        .map_err(|err| format!("无法解析地址：{err}"))?
        .next()
        .ok_or_else(|| "无法解析地址".to_string())
}

/// How many ports are looked at before giving up.
const SEARCH_LIMIT: usize = 2000;

/// A free port on `ip` to offer instead of the taken `port`: the first free one from
/// `port + 10000`, or from `port + 1` if that is past 65535.
pub(crate) fn suggest_port(ip: IpAddr, port: u16) -> Option<u16> {
    suggest_port_with(port, |candidate| TcpListener::bind(SocketAddr::new(ip, candidate)).is_ok())
}

/// [`suggest_port`] with the "is it free" check supplied.
pub(crate) fn suggest_port_with(port: u16, mut is_free: impl FnMut(u16) -> bool) -> Option<u16> {
    let start = u32::from(port) + 10_000;
    let start = if start > 65_535 { u32::from(port) + 1 } else { start };
    // Upwards from the start, then (if the top of the range is taken) the unprivileged ports below it.
    (start..=65_535)
        .chain(1_024..start.min(65_536))
        .filter_map(|candidate| u16::try_from(candidate).ok())
        .filter(|candidate| *candidate != port)
        .take(SEARCH_LIMIT)
        .find(|candidate| is_free(*candidate))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn suggestions_start_ten_thousand_above() {
        assert_eq!(suggest_port_with(8080, |_| true), Some(18080));
        assert_eq!(suggest_port_with(8080, |p| p > 18082), Some(18083));
    }

    #[test]
    fn suggestions_fall_back_to_the_next_port_when_ten_thousand_above_is_out_of_range() {
        assert_eq!(suggest_port_with(55_536, |_| true), Some(55_537), "55536 + 10000 = 65536");
        assert_eq!(suggest_port_with(60_000, |_| true), Some(60_001));
        assert_eq!(suggest_port_with(60_000, |p| p > 60_003), Some(60_004));
    }

    #[test]
    fn suggestions_wrap_below_when_the_top_is_full() {
        assert_eq!(suggest_port_with(65_535, |_| true), Some(1_024));
        assert_eq!(suggest_port_with(65_535, |p| p >= 2_000), Some(2_000));
        assert_eq!(suggest_port_with(65_000, |p| p < 65_000), Some(1_024));
    }

    #[test]
    fn no_suggestion_when_nothing_is_free() {
        assert_eq!(suggest_port_with(8080, |_| false), None);
    }

    #[test]
    fn a_taken_port_is_reported_with_a_free_suggestion() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let err = bind(&Endpoint::new("127.0.0.1", port)).unwrap_err();
        let RuleError::PortInUse { host, port: reported, suggestion } = err.clone() else { panic!("{err:?}") };
        assert_eq!((host.as_str(), reported), ("127.0.0.1", port));
        let suggestion = suggestion.expect("a free port is found");
        assert_ne!(suggestion, port);
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, suggestion)).is_ok(), "{suggestion} is free");
        assert_eq!(err.to_string(), format!("本地端口 {port} 已被占用"));
        assert_eq!(err.suggested_port(), Some(suggestion));
    }

    #[test]
    fn an_address_that_is_not_ours_cannot_be_listened_on() {
        let err = bind(&Endpoint::new("203.0.113.7", 40_000)).unwrap_err();
        assert!(matches!(err, RuleError::BindFailed { .. }), "{err:?}");
        assert!(err.to_string().contains("203.0.113.7"));
    }
}
