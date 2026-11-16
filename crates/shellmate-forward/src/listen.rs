//! Taking a local port, and finding another one when it is taken.

use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, ToSocketAddrs};

use tokio::net::TcpSocket;

use crate::addr::Endpoint;
use crate::state::RuleError;

/// Listens on `listen`, ready to be handed to tokio. A taken port gives
/// [`RuleError::PortInUse`] with a free port to offer instead.
///
/// A port counts as taken when another program listens on it on *any* address that clients of the
/// rule's address could mean, not only on the very address of the rule: the system allows
/// `127.0.0.1:P` to be bound while something listens on `0.0.0.0:P` (it sets `SO_REUSEADDR`), and
/// then local clients of `127.0.0.1:P` would silently reach the tunnel instead of that program.
pub(crate) fn bind(listen: &Endpoint) -> Result<TcpListener, RuleError> {
    let addr = resolve(listen).map_err(|reason| bind_failed(listen, reason))?;
    if shadows_a_listener(addr, &mut probe_bind) {
        return Err(port_in_use(listen, addr));
    }
    let listener = TcpListener::bind(addr).map_err(|err| match err.kind() {
        ErrorKind::AddrInUse => port_in_use(listen, addr),
        ErrorKind::PermissionDenied => {
            bind_failed(listen, "没有权限监听这个端口（1024 以下的端口需要管理员权限）".into())
        }
        ErrorKind::AddrNotAvailable => bind_failed(listen, "本机没有这个地址".into()),
        _ => bind_failed(listen, err.to_string()),
    })?;
    listener.set_nonblocking(true).map_err(|err| bind_failed(listen, err.to_string()))?;
    Ok(listener)
}

fn port_in_use(listen: &Endpoint, addr: SocketAddr) -> RuleError {
    RuleError::PortInUse {
        host: listen.host.clone(),
        port: listen.port,
        suggestion: suggest_port(addr.ip(), listen.port),
    }
}

/// The addresses a program may listen on that clients of any of the others could reach as well:
/// the wildcards and the loopbacks, both families (`localhost` resolves to either).
fn rivals(port: u16) -> [SocketAddr; 4] {
    [
        SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port),
        SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port),
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
        SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
    ]
}

/// Whether another program already listens on one of the other addresses of [`rivals`] (all but
/// `requested` itself, which the real bind takes care of). `probe` tries to bind an address; only
/// "address in use" counts, so a machine without IPv6 (`AddrNotAvailable`) is no obstacle.
fn shadows_a_listener(requested: SocketAddr, probe: &mut impl FnMut(SocketAddr) -> io::Result<()>) -> bool {
    rivals(requested.port())
        .into_iter()
        .filter(|rival| *rival != requested)
        .any(|rival| matches!(probe(rival), Err(err) if err.kind() == ErrorKind::AddrInUse))
}

/// Binds `addr` without listening, with `SO_REUSEADDR` like every listener does, and lets go
/// again: it only tells whether somebody listens there. Nothing can connect to it, and no firewall
/// asks about it.
fn probe_bind(addr: SocketAddr) -> io::Result<()> {
    let socket = if addr.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)
}

/// The local port to use for a forward that would like `port` on `host`: that very port when nobody listens
/// there, else the free port [`suggest_port`] offers (`None` if there is none, or `host` is not an address).
pub fn pick_local_port(host: &str, port: u16) -> Option<u16> {
    let ip: IpAddr = host.parse().ok()?;
    if is_free(SocketAddr::new(ip, port)) { Some(port) } else { suggest_port(ip, port) }
}

/// Whether nobody listens on `addr` or on an address that would shadow it.
fn is_free(addr: SocketAddr) -> bool {
    !shadows_a_listener(addr, &mut probe_bind) && probe_bind(addr).is_ok()
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
    suggest_port_with(port, |candidate| is_free(SocketAddr::new(ip, candidate)))
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
    use super::*;

    #[test]
    fn a_free_port_is_kept_and_a_taken_one_gets_a_suggestion() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let other = pick_local_port("127.0.0.1", port).expect("a free port is offered");
        assert_ne!(other, port);
        drop(taken);
        assert_eq!(pick_local_port("127.0.0.1", port), Some(port), "free again");
        assert_eq!(pick_local_port("not-an-ip", 80), None);
    }

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
        // A port below the range the system hands out at random, so that tests running at the
        // same time cannot take the ports around it (the suggestion is ten thousand above).
        let (taken, port) = (23_000u16..24_000)
            .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok().map(|listener| (listener, port)))
            .expect("a free port");
        let err = bind(&Endpoint::new("127.0.0.1", port)).unwrap_err();
        let RuleError::PortInUse { host, port: reported, suggestion } = err.clone() else { panic!("{err:?}") };
        assert_eq!((host.as_str(), reported), ("127.0.0.1", port));
        let suggestion = suggestion.expect("a free port is found");
        assert_eq!(suggestion, port + 10_000);
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, suggestion)).is_ok(), "{suggestion} is free");
        assert_eq!(err.to_string(), format!("本地端口 {port} 已被占用"));
        assert_eq!(err.suggested_port(), Some(suggestion));
        drop(taken);
    }

    #[test]
    fn an_address_that_is_not_ours_cannot_be_listened_on() {
        let err = bind(&Endpoint::new("203.0.113.7", 40_000)).unwrap_err();
        assert!(matches!(err, RuleError::BindFailed { .. }), "{err:?}");
        assert!(err.to_string().contains("203.0.113.7"));
    }

    /// The probe of the tests: these addresses are "in use", everything else can be bound.
    fn listening_on(taken: &[&str]) -> impl FnMut(SocketAddr) -> io::Result<()> {
        let taken: Vec<SocketAddr> = taken.iter().map(|text| text.parse().unwrap()).collect();
        move |addr| {
            if taken.contains(&addr) { Err(ErrorKind::AddrInUse.into()) } else { Ok(()) }
        }
    }

    #[test]
    fn a_listener_on_any_rival_address_shadows_the_rule() {
        let local: SocketAddr = "127.0.0.1:5432".parse().unwrap();
        // A database on all interfaces, on the IPv6 wildcard, or on the other loopback.
        for taken in ["0.0.0.0:5432", "[::]:5432", "[::1]:5432"] {
            assert!(shadows_a_listener(local, &mut listening_on(&[taken])), "{taken}");
        }
        assert!(!shadows_a_listener(local, &mut listening_on(&[])));
        // The rule's own address is the business of the real bind, not of the probe.
        assert!(!shadows_a_listener(local, &mut listening_on(&["127.0.0.1:5432"])));
        // Another port is no rival.
        assert!(!shadows_a_listener(local, &mut listening_on(&["0.0.0.0:5433", "[::1]:80"])));

        let v6: SocketAddr = "[::1]:5432".parse().unwrap();
        for taken in ["0.0.0.0:5432", "[::]:5432", "127.0.0.1:5432"] {
            assert!(shadows_a_listener(v6, &mut listening_on(&[taken])), "{taken}");
        }
        let lan: SocketAddr = "0.0.0.0:5432".parse().unwrap();
        for taken in ["[::]:5432", "127.0.0.1:5432", "[::1]:5432"] {
            assert!(shadows_a_listener(lan, &mut listening_on(&[taken])), "{taken}");
        }
    }

    #[test]
    fn every_other_address_of_the_port_is_probed_and_only_those() {
        for (requested, others) in [
            ("127.0.0.1:80", ["0.0.0.0:80", "[::]:80", "[::1]:80"]),
            ("[::1]:80", ["0.0.0.0:80", "[::]:80", "127.0.0.1:80"]),
            ("0.0.0.0:80", ["[::]:80", "127.0.0.1:80", "[::1]:80"]),
            ("[::]:80", ["0.0.0.0:80", "127.0.0.1:80", "[::1]:80"]),
        ] {
            let mut probed = Vec::new();
            let requested: SocketAddr = requested.parse().unwrap();
            assert!(!shadows_a_listener(requested, &mut |addr| {
                probed.push(addr.to_string());
                Ok(())
            }));
            probed.sort();
            let mut expected: Vec<String> = others.iter().map(|text| text.to_string()).collect();
            expected.sort();
            assert_eq!(probed, expected, "{requested}");
        }
    }

    #[test]
    fn a_machine_without_ipv6_is_no_obstacle() {
        let local: SocketAddr = "127.0.0.1:5432".parse().unwrap();
        let mut no_ipv6 = |addr: SocketAddr| {
            if addr.is_ipv6() { Err(io::Error::from(ErrorKind::AddrNotAvailable)) } else { Ok(()) }
        };
        assert!(!shadows_a_listener(local, &mut no_ipv6));
        // Other failures of a probe do not make up a conflict either.
        let mut broken = |_: SocketAddr| Err::<(), _>(io::Error::from(ErrorKind::PermissionDenied));
        assert!(!shadows_a_listener(local, &mut broken));
    }

    #[test]
    fn the_real_probe_sees_a_listener_on_the_same_loopback_address_and_lets_go_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = listener.local_addr().unwrap();
        assert!(matches!(probe_bind(taken), Err(err) if err.kind() == ErrorKind::AddrInUse));
        drop(listener);
        assert!(probe_bind(taken).is_ok(), "nothing is left behind by a probe");
        assert!(probe_bind(taken).is_ok());
    }
}
