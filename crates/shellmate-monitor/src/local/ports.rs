use super::runner;
use crate::ports::{Exposure, Family, Listener, PortSource, PortSources, Protocol, exposure};
use shellmate_ssh::ExecOutput;
use std::{collections::BTreeMap, io, net::IpAddr, time::Duration};
use tokio::process::Command;

fn word<'a>(rest: &mut &'a str) -> Option<&'a str> {
    let input = rest.trim_start();
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    *rest = &input[end..];
    (end != 0).then_some(&input[..end])
}

fn owner(rest: &str) -> Option<(&str, u32)> {
    let (name, mut tail) = rest.trim().rsplit_once(':')?;
    let pid = word(&mut tail)?.parse().ok()?;
    for width in [5, 8, 16, 8, 8] {
        let hex = word(&mut tail)?;
        if hex.len() != width || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some((name, pid))
}

fn valid_address(addr: &str) -> bool {
    let (host, zone) = addr.split_once('%').unwrap_or((addr, ""));
    (host == "*" || host.parse::<IpAddr>().is_ok())
        && zone.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn parse_netstat(text: &str, protocol: Protocol) -> io::Result<Vec<Listener>> {
    let invalid = || io::Error::other("无法识别 netstat 输出");
    let mut rows = Vec::new();
    for mut line in text.lines() {
        let family = match (protocol, word(&mut line)) {
            (Protocol::Tcp, Some("tcp4")) | (Protocol::Udp, Some("udp4")) => Family::V4,
            (Protocol::Tcp, Some("tcp6")) | (Protocol::Udp, Some("udp6")) => Family::V6,
            (Protocol::Tcp, Some("tcp46")) | (Protocol::Udp, Some("udp46")) => Family::Both,
            _ => continue,
        };
        word(&mut line).ok_or_else(invalid)?;
        word(&mut line).ok_or_else(invalid)?;
        let local = word(&mut line).ok_or_else(invalid)?;
        word(&mut line).ok_or_else(invalid)?;
        if protocol == Protocol::Tcp && word(&mut line) != Some("LISTEN") {
            continue;
        }
        let (addr, port) = local.rsplit_once('.').ok_or_else(invalid)?;
        if port == "*" || port == "0" {
            continue;
        }
        let port = port.parse().map_err(|_| invalid())?;
        if !valid_address(addr) {
            return Err(invalid());
        }
        for _ in 0..4 {
            word(&mut line).ok_or_else(invalid)?;
        }
        let (name, pid) = owner(line).ok_or_else(invalid)?;
        rows.push(Listener {
            protocol,
            family,
            addresses: vec![addr.into()],
            port,
            name: name.into(),
            user: None,
            pid,
            owner_start: None,
        });
    }
    Ok(rows)
}

fn lsof_binding(name: &str) -> Option<(&str, u16)> {
    let local = name.split_once("->").map_or(name, |(local, _)| local);
    let (addr, port) = local.rsplit_once(':')?;
    let addr = addr.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(addr);
    let port = port.parse().ok()?;
    (port != 0 && valid_address(addr)).then_some((addr, port))
}

fn parse_lsof(text: &str, protocol: Protocol) -> Vec<Listener> {
    let mut rows = Vec::new();
    let mut lines = text.lines().peekable();
    let (mut pid, mut command, mut user) = (None, "", None);
    while let Some(line) = lines.next() {
        if let Some(value) = line.strip_prefix('p') {
            (pid, command, user) = (value.parse::<u32>().ok(), "", None);
        } else if let Some(value) = line.strip_prefix('c') {
            command = value;
        } else if let Some(value) = line.strip_prefix('L') {
            user = Some(value);
        } else if line.starts_with('f') {
            let (mut family, mut transport, mut binding) = (None, None, None);
            while let Some(field) = lines.next_if(|field| !field.starts_with(['p', 'f'])) {
                match field {
                    "tIPv4" => family = Some(Family::V4),
                    "tIPv6" => family = Some(Family::V6),
                    "PTCP" => transport = Some(Protocol::Tcp),
                    "PUDP" => transport = Some(Protocol::Udp),
                    _ if field.starts_with('n') => binding = lsof_binding(&field[1..]),
                    _ => (),
                }
            }
            if transport == Some(protocol)
                && let (Some(pid), Some(family), Some((addr, port))) = (pid, family, binding)
            {
                rows.push(Listener {
                    protocol,
                    family,
                    addresses: vec![addr.into()],
                    port,
                    pid,
                    name: command.into(),
                    user: user.map(str::to_owned),
                    owner_start: None,
                });
            }
        }
    }
    rows
}

fn merge(rows: impl IntoIterator<Item = Listener>) -> Vec<Listener> {
    let mut groups = BTreeMap::new();
    for row in rows {
        let wildcard = exposure(&row.addresses[0]) == Exposure::All;
        groups
            .entry((row.protocol, row.port, row.pid, wildcard))
            .or_insert_with(Vec::new)
            .push(row);
    }
    let mut merged = Vec::new();
    for mut group in groups.into_values() {
        group.sort_by(|left, right| {
            left.addresses
                .cmp(&right.addresses)
                .then(left.family.cmp(&right.family))
        });
        let family = group[0].family;
        if group.iter().any(|row| row.family != family) {
            let mut first = group.remove(0);
            first.family = Family::Both;
            first.addresses.extend(group.into_iter().flat_map(|row| row.addresses));
            first.addresses.dedup();
            merged.push(first);
        } else {
            group.dedup_by(|left, right| left.addresses == right.addresses);
            merged.extend(group);
        }
    }
    merged
}

async fn run(source: PortSource, protocol: Protocol) -> io::Result<ExecOutput> {
    const TIMEOUT: Duration = Duration::from_secs(4);
    const LIMIT: usize = 16 << 20;
    match source {
        PortSource::Netstat => {
            let arg = if protocol == Protocol::Tcp { "tcp" } else { "udp" };
            runner::output_with_locale(Command::new("/usr/sbin/netstat").args(["-anv", "-p", arg]), TIMEOUT, LIMIT, runner::UTF8_LOCALE).await
        }
        PortSource::Lsof => {
            let mut cmd = Command::new("/usr/sbin/lsof");
            cmd.args(["-nP", "-FpcLnPtf"]);
            match protocol {
                Protocol::Tcp => cmd.args(["-iTCP", "-sTCP:LISTEN"]),
                Protocol::Udp => cmd.arg("-iUDP"),
            };
            runner::output_with_locale(&mut cmd, TIMEOUT, LIMIT, runner::UTF8_LOCALE).await
        }
    }
}

fn decode(source: PortSource, protocol: Protocol, out: ExecOutput) -> io::Result<Vec<Listener>> {
    let tool = match source {
        PortSource::Netstat => "netstat",
        PortSource::Lsof => "lsof",
    };
    if out.truncated {
        return Err(io::Error::other(format!("{tool} 输出过长")));
    }
    if source == PortSource::Lsof && out.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "{tool} {protocol:?}: {:?} {}",
            out.status,
            out.stderr_text().trim().chars().take(160).collect::<String>()
        )));
    }
    match source {
        PortSource::Netstat => parse_netstat(&out.stdout_text(), protocol),
        PortSource::Lsof => Ok(parse_lsof(&out.stdout_text(), protocol)),
    }
}

async fn read(
    protocol: Protocol,
    run: impl AsyncFn(PortSource, Protocol) -> io::Result<ExecOutput>,
) -> io::Result<(PortSource, Vec<Listener>)> {
    let rows = decode(PortSource::Netstat, protocol, run(PortSource::Netstat, protocol).await?)?;
    if !rows.is_empty() {
        return Ok((PortSource::Netstat, rows));
    }
    let rows = decode(PortSource::Lsof, protocol, run(PortSource::Lsof, protocol).await?)?;
    Ok((PortSource::Lsof, rows))
}

pub(crate) async fn sample() -> io::Result<(PortSources, Vec<Listener>)> {
    let ((tcp, tcp_rows), (udp, udp_rows)) = tokio::try_join!(read(Protocol::Tcp, run), read(Protocol::Udp, run))?;
    Ok((PortSources { tcp, udp }, merge(tcp_rows.into_iter().chain(udp_rows))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_ssh::ExecStatus;
    use std::cell::RefCell;

    const TCP: &str = include_str!("../../tests/fixtures/macos/netstat_tcp.txt");
    const UDP: &str = include_str!("../../tests/fixtures/macos/netstat_udp.txt");
    const LSOF_TCP: &str = include_str!("../../tests/fixtures/macos/lsof_tcp.txt");
    const LSOF_UDP: &str = include_str!("../../tests/fixtures/macos/lsof_udp.txt");

    #[test]
    fn lsof_preserves_process_fields_and_merges_dual_stack_listeners() {
        let rows = parse_lsof(LSOF_TCP, Protocol::Tcp);
        assert_eq!(rows.len(), 7);
        assert!(rows.iter().all(|row| row.user.as_deref() == Some("chen")));
        assert_eq!((rows[4].name.as_str(), rows[4].pid, rows[4].port), ("企业微信", 1379, 9884));
        assert_eq!((rows[5].name.as_str(), rows[5].pid, rows[5].port), ("企业微信", 1379, 9882));
        assert_eq!((rows[6].name.as_str(), rows[6].pid), ("Google Chrome H", 4242));
        assert_eq!((rows[2].family, rows[3].family), (Family::V4, Family::V6));
        let rows = merge(rows);
        let local = rows.iter().find(|row| row.port == 6379).unwrap();
        assert_eq!(local.family, Family::Both);
        assert_eq!(local.addresses, ["127.0.0.1", "::1"]);
        let all = rows.iter().find(|row| row.port == 49159).unwrap();
        assert_eq!(all.family, Family::Both);
        assert_eq!(all.addresses, ["*"]);
    }

    #[test]
    fn lsof_udp_skips_unbound_sockets_and_keeps_local_endpoints() {
        let rows = parse_lsof(LSOF_UDP, Protocol::Udp);
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].pid, rows[0].port), (719, 53968));
        assert_eq!(rows[1].endpoint(), "[fe80::1%lo0]:631");
        assert_eq!((rows[2].addresses[0].as_str(), rows[2].port), ("127.0.0.1", 53000));
    }

    fn output(code: u32, text: &str) -> ExecOutput {
        ExecOutput { stdout: text.as_bytes().into(), stderr: Vec::new(), status: ExecStatus::Code(code), truncated: false }
    }

    #[test]
    fn only_lsof_exit_one_means_no_matches() {
        assert!(decode(PortSource::Lsof, Protocol::Tcp, output(1, "")).unwrap().is_empty());
        assert!(decode(PortSource::Lsof, Protocol::Tcp, output(2, "")).is_err());
        assert!(decode(PortSource::Netstat, Protocol::Tcp, output(1, "")).is_err());
    }

    #[tokio::test]
    async fn fallback_only_for_protocols_without_netstat_listeners() {
        let established = TCP.lines().find(|line| line.contains("ESTABLISHED")).unwrap();
        let unbound = UDP.lines().find(|line| line.split_whitespace().nth(3) == Some("*.*")).unwrap();
        for (protocol, netstat, lsof, expected, count) in [
            (Protocol::Tcp, TCP, LSOF_TCP, PortSource::Netstat, 7),
            (Protocol::Udp, UDP, LSOF_UDP, PortSource::Netstat, 2),
            (Protocol::Tcp, "", LSOF_TCP, PortSource::Lsof, 7),
            (Protocol::Tcp, established, LSOF_TCP, PortSource::Lsof, 7),
            (Protocol::Udp, unbound, LSOF_UDP, PortSource::Lsof, 3),
        ] {
            let calls = RefCell::new(Vec::new());
            let (source, rows) = read(protocol, async |source, requested| {
                assert_eq!(requested, protocol);
                calls.borrow_mut().push(source);
                Ok(output(0, if source == PortSource::Netstat { netstat } else { lsof }))
            }).await.unwrap();
            assert_eq!((source, rows.len()), (expected, count));
            let calls = calls.into_inner();
            if expected == PortSource::Netstat {
                assert_eq!(calls, [PortSource::Netstat]);
            } else {
                assert_eq!(calls, [PortSource::Netstat, PortSource::Lsof]);
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires permission to run netstat and lsof on a real Mac"]
    async fn real_machine_tcp_listeners() {
        let baseline = runner::output_with_locale(
            Command::new("/usr/sbin/lsof").args(["-nP", "-Fp", "-iTCP", "-sTCP:LISTEN"]),
            Duration::from_secs(4),
            16 << 20,
            runner::UTF8_LOCALE,
        ).await.unwrap();
        assert!(!baseline.truncated);
        let pids = baseline.stdout_text().lines().filter(|line| line.starts_with('p')).count();
        assert_eq!(baseline.status.code(), Some(if pids == 0 { 1 } else { 0 }));
        let (sources, rows) = sample().await.unwrap();
        let tcp = rows.iter().filter(|row| row.protocol == Protocol::Tcp).count();
        eprintln!("lsof baseline: {pids} processes; collector: {tcp} TCP listeners ({:?})", sources.tcp);
        assert!(pids == 0 || tcp > 0);
    }

    #[test]
    fn tcp_listeners_preserve_names_and_ipv6() {
        let rows = parse_netstat(TCP, Protocol::Tcp).unwrap();
        assert_eq!(rows.len(), 7);
        assert_eq!(
            (rows[0].addresses[0].as_str(), rows[0].port, rows[0].family),
            ("fdfe:dcba:9876::", 62372, Family::V6)
        );
        assert_eq!((rows[1].addresses[0].as_str(), rows[1].port), ("198.18.0.1", 62371));
        assert_eq!((rows[2].family, rows[2].port), (Family::Both, 1053));
        assert_eq!((rows[3].name.as_str(), rows[3].pid), ("Google Chrome H", 4242));
        assert_eq!((rows[6].name.as_str(), rows[6].pid), ("企业微信", 1379));
        assert!(parse_netstat(&TCP.replace("00180 00000006", "invalid suffix"), Protocol::Tcp).is_err());
    }

    #[test]
    fn udp_keeps_bound_ports_only() {
        let rows = parse_netstat(UDP, Protocol::Udp).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].name.as_str(), rows[0].pid, rows[0].port, rows[0].family),
            ("replicatord", 30056, 55999, Family::Both)
        );
        assert_eq!(
            (rows[1].addresses[0].as_str(), rows[1].name.as_str()),
            ("fe80::1%lo0", "___go_build_yuny")
        );
    }

    #[test]
    fn merges_families_without_losing_bindings_or_owners() {
        let merged = merge(parse_netstat(TCP, Protocol::Tcp).unwrap());
        let siblings: Vec<_> = merged.iter().filter(|row| row.port == 9001).collect();
        assert_eq!(siblings.len(), 3);
        assert!(
            siblings
                .iter()
                .any(|row| row.family == Family::Both && row.addresses == ["127.0.0.1", "::1"])
        );
        assert!(siblings.iter().any(|row| row.pid == 4242 && row.addresses == ["*"]));
        assert!(siblings.iter().any(|row| row.pid == 1379));
    }
}
