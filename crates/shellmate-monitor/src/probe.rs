//! What the monitor learns about a server before it collects anything (see
//! [`probe_script`](crate::script::probe_script)): the system, who the login user is and what sudo
//! allows, systemd, docker and podman, the units of the kernel's counters, and the user names.

use std::collections::HashMap;

use crate::parse::parse_passwd;

/// What the login user may do as root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sudo {
    /// The login user is root.
    Root,
    /// `sudo -n true` works: no password needed.
    Passwordless,
    /// `sudo` exists but asks for a password (the app never asks for one).
    NeedsPassword,
    /// There is no `sudo`.
    Unavailable,
}

/// Which container tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeKind {
    /// `docker`
    Docker,
    /// `podman`
    Podman,
}

impl RuntimeKind {
    /// The command name.
    pub fn command(self) -> &'static str {
        match self {
            RuntimeKind::Docker => "docker",
            RuntimeKind::Podman => "podman",
        }
    }
}

/// Whether the login user can use a container tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerAccess {
    /// `docker version` works as the login user.
    User,
    /// It only works with `sudo -n`.
    Sudo,
    /// The user may not use the socket (not in the `docker` group).
    Denied,
    /// The command exists but the daemon does not answer.
    NoDaemon,
    /// It failed in some other way.
    Error,
}

/// A container tool on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerRuntime {
    /// Which one.
    pub kind: RuntimeKind,
    /// Where the command is.
    pub path: String,
    /// What the login user may do with it.
    pub access: ContainerAccess,
    /// The version it reported, when it answered.
    pub version: Option<String>,
}

/// systemd on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Systemd {
    /// The version `systemctl --version` printed (`245`), if `systemctl` is there.
    pub version: Option<u32>,
    /// The system was booted by systemd (`/run/systemd/system` exists), so services can be managed.
    pub running: bool,
}

/// Everything the probe found out.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    /// `PRETTY_NAME` of `/etc/os-release`.
    pub os_name: Option<String>,
    /// `uname -sr`: `Linux 5.15.0-91-generic`.
    pub kernel: String,
    /// A Linux with a readable `/proc`: the only kind of server that can be monitored.
    pub is_linux: bool,
    /// Seconds since the server booted, when the probe ran.
    pub uptime_secs: Option<f64>,
    /// The login user's id.
    pub uid: Option<u32>,
    /// The login user's name.
    pub user: String,
    /// What sudo allows.
    pub sudo: Sudo,
    /// systemd, if the server has it.
    pub systemd: Option<Systemd>,
    /// docker and podman, as far as they are installed.
    pub containers: Vec<ContainerRuntime>,
    /// The size of a memory page in bytes (what the RSS of a process is counted in).
    pub page_size: u64,
    /// Clock ticks per second (the unit of CPU times).
    pub clk_tck: u64,
    /// User ids and names.
    pub users: HashMap<u32, String>,
}

/// The page size assumed when the server does not say (almost every Linux machine, but not ARM servers
/// with 16 or 64 KiB pages: those answer).
pub const DEFAULT_PAGE_SIZE: u64 = 4096;
/// The tick rate assumed when the server does not say (`USER_HZ` is 100 on every architecture that
/// matters).
pub const DEFAULT_CLK_TCK: u64 = 100;

impl Probe {
    /// A probe that knows nothing (the starting point of the parser; also handy in tests).
    pub fn unknown() -> Self {
        Self {
            os_name: None,
            kernel: String::new(),
            is_linux: false,
            uptime_secs: None,
            uid: None,
            user: String::new(),
            sudo: Sudo::Unavailable,
            systemd: None,
            containers: Vec::new(),
            page_size: DEFAULT_PAGE_SIZE,
            clk_tck: DEFAULT_CLK_TCK,
            users: HashMap::new(),
        }
    }

    /// Whether the collector can run here.
    pub fn can_collect(&self) -> bool {
        self.is_linux
    }

    /// The container tool to use: the first one the login user can use (directly or with sudo), docker
    /// before podman.
    pub fn usable_container_runtime(&self) -> Option<&ContainerRuntime> {
        self.containers.iter().find(|runtime| matches!(runtime.access, ContainerAccess::User | ContainerAccess::Sudo))
    }

    /// The name of a user id: from `passwd`, or the number if nobody has that id.
    pub fn user_name(&self, uid: u32) -> String {
        self.users.get(&uid).cloned().unwrap_or_else(|| uid.to_string())
    }
}

/// Splits the output of the probe into its sections.
fn sections<'a>(prefix: &str, text: &'a str) -> HashMap<&'a str, Vec<&'a str>> {
    let mut sections: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut current: Option<&str> = None;
    let mut finished = false;
    for line in text.lines() {
        if finished {
            break;
        }
        match line.strip_prefix(prefix) {
            Some("end") => finished = true,
            Some(name) => {
                current = Some(name);
                sections.entry(name).or_default();
            }
            None => {
                if let Some(name) = current {
                    sections.entry(name).or_default().push(line);
                }
            }
        }
    }
    sections
}

/// Reads what the probe script printed. A section that is missing or garbled leaves the default (so a
/// server that cannot run something does not fail the whole probe); the output of a probe that did not
/// get as far as its end is still used.
pub fn parse_probe(token: &str, text: &str) -> Probe {
    let sections = sections(&format!("@@{token} "), text);
    let first =
        |name: &str| sections.get(name).and_then(|lines| lines.iter().map(|l| l.trim()).find(|l| !l.is_empty()));
    let mut probe = Probe::unknown();

    probe.os_name = sections.get("os").and_then(|lines| {
        lines.iter().find_map(|line| line.strip_prefix("PRETTY_NAME=")).map(unquote).filter(|name| !name.is_empty())
    });
    probe.kernel = first("kernel").unwrap_or_default().to_string();
    probe.is_linux = probe.kernel.starts_with("Linux") && first("procfs") == Some("yes");
    probe.uptime_secs =
        first("uptime").and_then(|word| word.parse::<f64>().ok()).filter(|secs| secs.is_finite() && *secs >= 0.0);

    probe.uid = sections.get("user").and_then(|lines| lines.first()).and_then(|line| line.trim().parse().ok());
    probe.user =
        sections.get("user").and_then(|lines| lines.get(1)).map(|line| line.trim().to_string()).unwrap_or_default();

    probe.sudo = if probe.uid == Some(0) {
        Sudo::Root
    } else {
        match first("sudo") {
            Some("nopasswd") => Sudo::Passwordless,
            Some("password") => Sudo::NeedsPassword,
            _ => Sudo::Unavailable,
        }
    };

    if let Some(lines) = sections.get("systemd") {
        let version = lines
            .first()
            .and_then(|line| line.strip_prefix("systemd "))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok());
        let running = lines.iter().any(|line| line.trim() == "running");
        if version.is_some() || running {
            probe.systemd = Some(Systemd { version, running });
        }
    }

    for line in sections.get("containers").map(Vec::as_slice).unwrap_or_default() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let kind = |name: &str| match name {
            "docker" => Some(RuntimeKind::Docker),
            "podman" => Some(RuntimeKind::Podman),
            _ => None,
        };
        match words.as_slice() {
            ["bin", name, path @ ..] if kind(name).is_some() && !path.is_empty() => {
                probe.containers.push(ContainerRuntime {
                    kind: kind(name).expect("checked"),
                    path: path.join(" "),
                    access: ContainerAccess::Error,
                    version: None,
                })
            }
            ["access", name, who, outcome, rest @ ..] => {
                let Some(runtime) = probe.containers.iter_mut().find(|runtime| Some(runtime.kind) == kind(name)) else {
                    continue;
                };
                let access = match (*who, *outcome) {
                    ("user", "ok") => ContainerAccess::User,
                    ("sudo", "ok") => ContainerAccess::Sudo,
                    ("user", "denied") => ContainerAccess::Denied,
                    ("user", "nodaemon") => ContainerAccess::NoDaemon,
                    ("user", _) => ContainerAccess::Error,
                    _ => continue,
                };
                // `sudo` only counts if the plain attempt did not already work.
                if runtime.access != ContainerAccess::User {
                    runtime.access = access;
                    runtime.version = rest
                        .first()
                        .map(|v| v.to_string())
                        .filter(|_| matches!(access, ContainerAccess::User | ContainerAccess::Sudo));
                }
            }
            _ => {}
        }
    }

    probe.page_size = first("page")
        .and_then(|word| word.parse().ok())
        .filter(|size: &u64| size.is_power_of_two() && *size >= 1024)
        .unwrap_or(DEFAULT_PAGE_SIZE);
    probe.clk_tck = first("clk")
        .and_then(|word| word.parse().ok())
        .filter(|hz: &u64| (1..=10_000).contains(hz))
        .unwrap_or(DEFAULT_CLK_TCK);
    probe.users = sections.get("passwd").map(|lines| parse_passwd(&lines.join("\n"))).unwrap_or_default();
    probe
}

/// `"Ubuntu 22.04.3 LTS"` or `'x'` or `x`: what an os-release value looks like.
fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote).and_then(|rest| rest.strip_suffix(quote)) {
            return inner.replace("\\\"", "\"").replace("\\\\", "\\");
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UBUNTU: &str = "@@t os\nPRETTY_NAME=\"Ubuntu 22.04.3 LTS\"\n@@t kernel\nLinux 5.15.0-91-generic\n@@t procfs\nyes\n@@t uptime\n1234567.89\n@@t user\n1001\ndeploy\n@@t sudo\nnopasswd\n@@t systemd\nsystemd 249\nrunning\n@@t containers\nbin docker /usr/bin/docker\naccess docker user denied\naccess docker sudo ok 24.0.7\nbin podman /usr/bin/podman\naccess podman user nodaemon\n@@t page\n4096\n@@t clk\n100\n@@t passwd\nroot:x:0:0:root:/root:/bin/bash\ndeploy:x:1001:1001::/home/deploy:/bin/bash\n@@t end\n";

    #[test]
    fn a_typical_ubuntu_server() {
        let probe = parse_probe("t", UBUNTU);
        assert_eq!(probe.os_name.as_deref(), Some("Ubuntu 22.04.3 LTS"));
        assert_eq!(probe.kernel, "Linux 5.15.0-91-generic");
        assert!(probe.is_linux && probe.can_collect());
        assert_eq!(probe.uptime_secs, Some(1234567.89));
        assert_eq!((probe.uid, probe.user.as_str()), (Some(1001), "deploy"));
        assert_eq!(probe.sudo, Sudo::Passwordless);
        assert_eq!(probe.systemd, Some(Systemd { version: Some(249), running: true }));
        assert_eq!(probe.containers.len(), 2);
        assert_eq!(
            probe.containers[0],
            ContainerRuntime {
                kind: RuntimeKind::Docker,
                path: "/usr/bin/docker".into(),
                access: ContainerAccess::Sudo,
                version: Some("24.0.7".into())
            }
        );
        assert_eq!(probe.containers[1].access, ContainerAccess::NoDaemon);
        assert_eq!(probe.usable_container_runtime().unwrap().kind, RuntimeKind::Docker);
        assert_eq!((probe.page_size, probe.clk_tck), (4096, 100));
        assert_eq!(probe.user_name(1001), "deploy");
        assert_eq!(probe.user_name(4242), "4242");
    }

    #[test]
    fn root_needs_no_sudo_and_a_plain_docker_group_member_needs_none_for_docker() {
        let text = "@@t user\n0\nroot\n@@t sudo\nnone\n@@t containers\nbin docker /usr/local/bin/docker\naccess docker user ok 26.1.0\n@@t end\n";
        let probe = parse_probe("t", text);
        assert_eq!(probe.sudo, Sudo::Root);
        assert_eq!(probe.containers[0].access, ContainerAccess::User);
        assert_eq!(probe.containers[0].version.as_deref(), Some("26.1.0"));
    }

    #[test]
    fn an_alpine_container_without_systemd_or_docker() {
        let text = "@@t os\nPRETTY_NAME=\"Alpine Linux v3.24\"\n@@t kernel\nLinux 7.0.11-orbstack\n@@t procfs\nyes\n@@t uptime\n190057.84\n@@t user\n1000\ntester\n@@t sudo\npassword\n@@t systemd\n\n@@t containers\n@@t page\n4096\n@@t clk\n100\n@@t passwd\ntester:x:1000:100:Linux User,,,:/home/tester:/bin/sh\n@@t end\n";
        let probe = parse_probe("t", text);
        assert_eq!(probe.os_name.as_deref(), Some("Alpine Linux v3.24"));
        assert_eq!(probe.sudo, Sudo::NeedsPassword);
        assert_eq!(probe.systemd, None);
        assert!(probe.containers.is_empty() && probe.usable_container_runtime().is_none());
    }

    #[test]
    fn other_systems_are_recognised_as_not_linux() {
        let probe =
            parse_probe("t", "@@t kernel\nDarwin 23.1.0\n@@t procfs\n@@t user\n501\nme\n@@t sudo\npassword\n@@t end\n");
        assert!(!probe.is_linux && !probe.can_collect());
        assert_eq!(probe.kernel, "Darwin 23.1.0");
        // A Linux kernel without a readable /proc (a restricted container) cannot be monitored either.
        let probe = parse_probe("t", "@@t kernel\nLinux 6.1\n@@t procfs\n@@t end\n");
        assert!(!probe.is_linux);
    }

    #[test]
    fn missing_garbled_or_odd_sections_leave_defaults() {
        let probe = parse_probe("t", "");
        assert_eq!(probe, Probe::unknown());
        let text = "@@t page\n0\n@@t clk\n99999999\n@@t user\nx\n@@t uptime\nsoon\n@@t os\nNAME=x\n@@t containers\nbin docker\naccess docker user\naccess nothing user ok 1\nbin lxc /usr/bin/lxc\n";
        let probe = parse_probe("t", text);
        assert_eq!((probe.page_size, probe.clk_tck), (DEFAULT_PAGE_SIZE, DEFAULT_CLK_TCK));
        assert_eq!(probe.uid, None);
        assert_eq!(probe.uptime_secs, None);
        assert_eq!(probe.os_name, None);
        assert!(probe.containers.is_empty());
        // A 64 KiB page (ARM servers) is believed.
        assert_eq!(parse_probe("t", "@@t page\n65536\n").page_size, 65536);
        assert_eq!(parse_probe("t", "@@t page\n3000\n").page_size, DEFAULT_PAGE_SIZE, "not a power of two");
    }

    #[test]
    fn what_a_login_banner_or_the_wrong_token_cannot_change() {
        let text = format!("Welcome to the machine\n{UBUNTU}@@t os\nPRETTY_NAME=\"After the end\"\n");
        assert_eq!(
            parse_probe("t", &text).os_name.as_deref(),
            Some("Ubuntu 22.04.3 LTS"),
            "nothing counts after the end line"
        );
        assert_eq!(parse_probe("other", UBUNTU), Probe::unknown(), "markers with another token are plain text");
    }

    #[test]
    fn os_release_quoting() {
        assert_eq!(unquote("\"Debian GNU/Linux 12 (bookworm)\""), "Debian GNU/Linux 12 (bookworm)");
        assert_eq!(unquote("'Odd OS'"), "Odd OS");
        assert_eq!(unquote("Plain"), "Plain");
        assert_eq!(unquote("\"with \\\"quotes\\\"\""), "with \"quotes\"");
    }

    #[test]
    fn a_denied_docker_with_sudo_that_also_fails_stays_denied() {
        let text = "@@t user\n1000\nme\n@@t sudo\nnopasswd\n@@t containers\nbin docker /usr/bin/docker\naccess docker user denied\n@@t end\n";
        let probe = parse_probe("t", text);
        assert_eq!(probe.containers[0].access, ContainerAccess::Denied);
        assert!(probe.usable_container_runtime().is_none());
    }
}
