//! Containers: the commands that ask docker (or podman) and the parsers of the answers.
//!
//! Everything goes through the command line tool and `--format '{{json .}}'`, not the socket: that works
//! with podman-docker, rootless setups and `DOCKER_HOST`, and a permission problem is solved by `sudo -n`
//! like any other. The parsers read the JSON as it comes and take what is there: docker and podman name
//! their fields a little differently, and a field that is missing is just not known.
//!
//! * `ps -a --no-trunc --format '{{json .}}'`: which containers there are, their image, state, ports and
//!   labels (compose projects);
//! * `inspect --format '<template>'` for the numbers `ps` does not have: restarts, whether the last stop was
//!   an out-of-memory kill, the memory limit, the IP addresses in their networks;
//! * `stats --format '{{json .}}'` (a stream, only while somebody looks at the page): CPU and memory;
//! * `events --format '{{json .}}' --filter type=container` (a stream): something changed, ask again;
//! * `system df --format '{{json .}}'` and `images --filter dangling=true --format '{{json .}}'` for the
//!   header and the clean-up.

use std::collections::HashMap;

use serde_json::Value;

use crate::commands::{BuildError, Command};

/// The template of `inspect`: one JSON object per container, with just what the page needs.
pub const INSPECT_TEMPLATE: &str = r#"{"id":{{json .Id}},"name":{{json .Name}},"restarts":{{json .RestartCount}},"oom":{{json .State.OOMKilled}},"restarting":{{json .State.Restarting}},"memory":{{json .HostConfig.Memory}},"labels":{{json .Config.Labels}},"nets":{{json .NetworkSettings.Networks}}}"#;

/// How many containers one `inspect` call is asked about.
pub const INSPECT_BATCH: usize = 100;

fn tool(runtime: &str, args: &[&str]) -> Command {
    let mut argv = vec![runtime.to_string()];
    argv.extend(args.iter().map(|word| (*word).to_string()));
    Command::new(argv).expect("a fixed command")
}

/// `docker ps -a --no-trunc --format '{{json .}}'`.
pub fn ps(runtime: &str) -> Command {
    tool(runtime, &["ps", "-a", "--no-trunc", "--format", "{{json .}}"])
}

/// `docker inspect --format '<template>' id id …`.
pub fn inspect(runtime: &str, ids: &[String]) -> Result<Command, BuildError> {
    let mut argv = vec![runtime.to_string(), "inspect".into(), "--format".into(), INSPECT_TEMPLATE.into()];
    for id in ids {
        if id.is_empty() {
            return Err(BuildError::Empty);
        }
        if id.starts_with('-') {
            return Err(BuildError::LooksLikeOption);
        }
        argv.push(id.clone());
    }
    Command::new(argv)
}

/// `docker stats --format '{{json .}}'`: runs until it is stopped.
pub fn stats(runtime: &str) -> Command {
    tool(runtime, &["stats", "--format", "{{json .}}"])
}

/// `docker events --format '{{json .}}' --filter type=container`: runs until it is stopped.
pub fn events(runtime: &str) -> Command {
    tool(runtime, &["events", "--format", "{{json .}}", "--filter", "type=container"])
}

/// `docker system df --format '{{json .}}'`.
pub fn system_df(runtime: &str) -> Command {
    tool(runtime, &["system", "df", "--format", "{{json .}}"])
}

/// `docker images --filter dangling=true --format '{{json .}}'`.
pub fn dangling_images(runtime: &str) -> Command {
    tool(runtime, &["images", "--filter", "dangling=true", "--format", "{{json .}}"])
}

/// What the terminal tab of a container runs: `docker exec -it NAME sh -c 'command -v bash >/dev/null && exec bash || exec sh'`
/// (bash if the container has it, sh otherwise).
pub fn exec_shell(runtime: &str, name: &str) -> Result<Command, BuildError> {
    if name.is_empty() {
        return Err(BuildError::Empty);
    }
    if name.starts_with('-') {
        return Err(BuildError::LooksLikeOption);
    }
    Command::new([runtime, "exec", "-it", name, "sh", "-c", "command -v bash >/dev/null && exec bash || exec sh"])
}

/// `docker logs -f --tail 100 -t NAME` (without `-f` for a container that does not run: it would not end).
pub fn logs(runtime: &str, name: &str, follow: bool) -> Result<Command, BuildError> {
    if name.is_empty() {
        return Err(BuildError::Empty);
    }
    if name.starts_with('-') {
        return Err(BuildError::LooksLikeOption);
    }
    let mut argv = vec![runtime, "logs"];
    if follow {
        argv.push("-f");
    }
    argv.extend(["--tail", "100", "-t", name]);
    Command::new(argv)
}

// ---- the model -------------------------------------------------------------------------------

/// What a container is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerState {
    /// Up.
    Running,
    /// Being restarted by its restart policy.
    Restarting,
    /// Stopped.
    Exited,
    /// Made but never started.
    Created,
    /// Frozen.
    Paused,
    /// Broken beyond use.
    Dead,
    /// Anything else the tool says.
    Other(String),
}

impl ContainerState {
    /// Reads the state word of `ps`.
    pub fn parse(word: &str) -> Self {
        match word.trim().to_ascii_lowercase().as_str() {
            "running" => ContainerState::Running,
            "restarting" => ContainerState::Restarting,
            "exited" | "stopped" => ContainerState::Exited,
            "created" | "configured" | "initialized" => ContainerState::Created,
            "paused" => ContainerState::Paused,
            "dead" => ContainerState::Dead,
            other => ContainerState::Other(other.to_string()),
        }
    }

    /// Whether commands like `exec` and a followed log make sense.
    pub fn is_running(&self) -> bool {
        matches!(self, ContainerState::Running | ContainerState::Restarting | ContainerState::Paused)
    }
}

/// A port of a container: published on the host, or only inside its network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortMapping {
    /// The address it is published on (`0.0.0.0`, `127.0.0.1`), if it is published.
    pub host_ip: Option<String>,
    /// The port on the host, if it is published.
    pub host_port: Option<u16>,
    /// The port in the container.
    pub container_port: u16,
    /// `tcp` or `udp`.
    pub protocol: String,
}

impl PortMapping {
    /// Published on the host.
    pub fn is_published(&self) -> bool {
        self.host_port.is_some()
    }

    /// What the chip says: `127.0.0.1:3000→3000`, `18080→80` (published on every address), `6379/tcp`
    /// (not published).
    pub fn label(&self) -> String {
        let udp = if self.protocol == "udp" { "/udp" } else { "" };
        match self.host_port {
            Some(host_port) => match self.specific_host_ip() {
                Some(ip) => format!("{ip}:{host_port}→{}{udp}", self.container_port),
                None => format!("{host_port}→{}{udp}", self.container_port),
            },
            None => format!("{}/{}", self.container_port, self.protocol),
        }
    }

    /// The address a published port is bound to when it is one address (not every address).
    pub fn specific_host_ip(&self) -> Option<&str> {
        self.host_ip.as_deref().filter(|ip| !matches!(*ip, "" | "0.0.0.0" | "::" | "[::]"))
    }

    /// Where a forward to this port goes (as seen from the server): the published address (`127.0.0.1` when
    /// it is published on every address) and port; `None` for a port that is not published (it is reached
    /// through the container's own address instead).
    pub fn published_target(&self) -> Option<(String, u16)> {
        let port = self.host_port?;
        Some((self.specific_host_ip().unwrap_or("127.0.0.1").to_string(), port))
    }
}

/// The address of a container in one of its networks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerNetwork {
    /// The network's name.
    pub name: String,
    /// The container's address in it.
    pub ip: String,
}

/// A container, as the page shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Container {
    /// The full id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// The image it was made from.
    pub image: String,
    /// What it is doing.
    pub state: ContainerState,
    /// What `ps` says in words (`Up 11 hours`, `Exited (0) 2 weeks ago`).
    pub status: String,
    /// Its ports.
    pub ports: Vec<PortMapping>,
    /// The compose project it belongs to.
    pub project: Option<String>,
    /// The compose files of the project (`com.docker.compose.project.config_files`).
    pub config_files: Option<String>,
    /// The compose service it is.
    pub service: Option<String>,
    /// How often the restart policy restarted it.
    pub restart_count: u32,
    /// The last stop was an out-of-memory kill.
    pub oom_killed: bool,
    /// The restart policy is about to start it again (or has just).
    pub restarting: bool,
    /// The memory limit in bytes, if it has one.
    pub memory_limit: Option<u64>,
    /// Its addresses in its networks.
    pub networks: Vec<ContainerNetwork>,
    /// How long it has been up, if `ps` said (`Up 3 seconds`).
    pub up_secs: Option<u64>,
    /// CPU in percent of one core (from `stats`, while the page is open).
    pub cpu_percent: Option<f64>,
    /// Memory in use in bytes (from `stats`).
    pub mem_used: Option<u64>,
}

impl Container {
    /// The container's own address: the first network that gave it one.
    pub fn ip(&self) -> Option<&str> {
        self.networks.iter().map(|net| net.ip.as_str()).find(|ip| !ip.is_empty())
    }

    /// The restart policy keeps starting it: it is restarting now, or it restarted at least twice and has
    /// been up for less than five minutes. A container that restarted a while ago and runs fine is not.
    pub fn is_restart_looping(&self) -> bool {
        self.restarting
            || self.state == ContainerState::Restarting
            || (self.restart_count >= 2 && self.up_secs.is_some_and(|secs| secs < 300))
    }
}

/// A row of `ps`.
#[derive(Clone, Debug, PartialEq)]
pub struct PsEntry {
    /// The full id.
    pub id: String,
    /// The first name.
    pub name: String,
    /// The image.
    pub image: String,
    /// The state word.
    pub state: String,
    /// The status text.
    pub status: String,
    /// The ports.
    pub ports: Vec<PortMapping>,
    /// The labels.
    pub labels: HashMap<String, String>,
}

fn field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(*key)).filter(|value| !value.is_null())
}

fn string_of(value: &Value, keys: &[&str]) -> Option<String> {
    match field(value, keys)? {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => items.first().and_then(Value::as_str).map(str::to_string),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The containers of `ps -a --no-trunc --format '{{json .}}'`: one JSON object per line. Lines that are
/// not an object with an id (a warning, an empty line) are skipped.
pub fn parse_ps(text: &str) -> Vec<PsEntry> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter_map(|value| {
            let id = string_of(&value, &["ID", "Id", "id"])?;
            let name = string_of(&value, &["Names", "Name", "names"]).unwrap_or_default();
            // docker lists the names of linked containers comma-separated, and puts a `/` in front of some.
            let name = name.split(',').next().unwrap_or("").trim_start_matches('/').to_string();
            let ports = match field(&value, &["Ports", "ports"]) {
                Some(Value::String(text)) => parse_ports(text),
                Some(Value::Array(items)) => items.iter().filter_map(port_of_podman).collect(),
                _ => Vec::new(),
            };
            Some(PsEntry {
                id,
                name,
                image: string_of(&value, &["Image", "image"]).unwrap_or_default(),
                state: string_of(&value, &["State", "state"]).unwrap_or_default(),
                status: string_of(&value, &["Status", "status"]).unwrap_or_default(),
                ports,
                labels: labels_of(field(&value, &["Labels", "labels"])),
            })
        })
        .collect()
}

/// `{"a":"1"}` (an object) or `a=1,b=2` (docker's text): the labels of a container. A value with a comma
/// in it (a list of compose files) is kept whole: a piece without an `=` belongs to the label before it.
fn labels_of(value: Option<&Value>) -> HashMap<String, String> {
    match value {
        Some(Value::Object(map)) => {
            map.iter().filter_map(|(key, value)| value.as_str().map(|v| (key.clone(), v.to_string()))).collect()
        }
        Some(Value::String(text)) => parse_label_text(text),
        _ => HashMap::new(),
    }
}

fn parse_label_text(text: &str) -> HashMap<String, String> {
    let mut labels: Vec<(String, String)> = Vec::new();
    for piece in text.split(',') {
        match piece.split_once('=') {
            // A key has no spaces or slashes of a path in it.
            Some((key, value)) if !key.is_empty() && !key.contains(['/', ' ']) => {
                labels.push((key.to_string(), value.to_string()))
            }
            _ => {
                if let Some((_, value)) = labels.last_mut() {
                    value.push(',');
                    value.push_str(piece);
                }
            }
        }
    }
    labels.into_iter().collect()
}

/// `0.0.0.0:18080->80/tcp, [::]:18080->80/tcp, 6379/tcp, 127.0.0.1:8000-8001->8000-8001/tcp`. The same port
/// published on IPv4 and IPv6 is one port (the IPv4 one is kept).
pub fn parse_ports(text: &str) -> Vec<PortMapping> {
    let mut ports: Vec<PortMapping> = Vec::new();
    for item in text.split(',').map(str::trim).filter(|item| !item.is_empty()) {
        for port in ports_of_item(item) {
            let same = |other: &PortMapping| {
                other.host_port == port.host_port
                    && other.container_port == port.container_port
                    && other.protocol == port.protocol
            };
            if !ports.iter().any(same) {
                ports.push(port);
            }
        }
    }
    ports
}

fn ports_of_item(item: &str) -> Vec<PortMapping> {
    let (left, right) = match item.split_once("->") {
        Some((host, container)) => (Some(host), container),
        None => (None, item),
    };
    let (container_range, protocol) = right.split_once('/').unwrap_or((right, "tcp"));
    let container = port_range(container_range);
    let Some(host) = left else {
        return container
            .into_iter()
            .map(|container_port| PortMapping {
                host_ip: None,
                host_port: None,
                container_port,
                protocol: protocol.into(),
            })
            .collect();
    };
    let (ip, host_range) = host.rsplit_once(':').unwrap_or(("", host));
    let ip = ip.trim_start_matches('[').trim_end_matches(']').to_string();
    let host_ports = port_range(host_range);
    if host_ports.len() != container.len() {
        return Vec::new();
    }
    host_ports
        .into_iter()
        .zip(container)
        .map(|(host_port, container_port)| PortMapping {
            host_ip: Some(ip.clone()),
            host_port: Some(host_port),
            container_port,
            protocol: protocol.into(),
        })
        .collect()
}

/// `80` or `8000-8002`.
fn port_range(text: &str) -> Vec<u16> {
    match text.split_once('-') {
        Some((from, to)) => match (from.trim().parse::<u16>(), to.trim().parse::<u16>()) {
            (Ok(from), Ok(to)) if from <= to && usize::from(to - from) < 64 => (from..=to).collect(),
            _ => Vec::new(),
        },
        None => text.trim().parse::<u16>().map(|port| vec![port]).unwrap_or_default(),
    }
}

/// podman's `{"host_ip":"","container_port":80,"host_port":8080,"protocol":"tcp"}`.
fn port_of_podman(value: &Value) -> Option<PortMapping> {
    let number = |keys: &[&str]| field(value, keys).and_then(Value::as_u64).and_then(|n| u16::try_from(n).ok());
    let container_port = number(&["container_port", "ContainerPort"])?;
    let host_port = number(&["host_port", "HostPort"]).filter(|port| *port > 0);
    Some(PortMapping {
        host_ip: host_port.map(|_| string_of(value, &["host_ip", "HostIp"]).unwrap_or_default()),
        host_port,
        container_port,
        protocol: string_of(value, &["protocol", "Protocol"]).unwrap_or_else(|| "tcp".into()),
    })
}

/// What `inspect` (with [`INSPECT_TEMPLATE`]) says about one container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectEntry {
    /// The full id.
    pub id: String,
    /// The restart policy's count.
    pub restarts: u32,
    /// The last stop was an out-of-memory kill.
    pub oom_killed: bool,
    /// It is being restarted.
    pub restarting: bool,
    /// The memory limit in bytes (0 means none).
    pub memory: u64,
    /// The labels.
    pub labels: HashMap<String, String>,
    /// Its addresses.
    pub networks: Vec<ContainerNetwork>,
}

/// The lines of `inspect --format`: one JSON object per container.
pub fn parse_inspect(text: &str) -> Vec<InspectEntry> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter_map(|value| {
            let id = string_of(&value, &["id"])?;
            let mut networks: Vec<ContainerNetwork> = match field(&value, &["nets"]) {
                Some(Value::Object(map)) => map
                    .iter()
                    .map(|(name, net)| ContainerNetwork {
                        name: name.clone(),
                        ip: string_of(net, &["IPAddress", "IPv4Address"]).unwrap_or_default(),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            networks.sort_by(|a, b| a.name.cmp(&b.name));
            Some(InspectEntry {
                id,
                restarts: field(&value, &["restarts"])
                    .and_then(Value::as_u64)
                    .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX)),
                oom_killed: field(&value, &["oom"]).and_then(Value::as_bool).unwrap_or(false),
                restarting: field(&value, &["restarting"]).and_then(Value::as_bool).unwrap_or(false),
                memory: field(&value, &["memory"]).and_then(Value::as_u64).unwrap_or(0),
                labels: labels_of(field(&value, &["labels"])),
                networks,
            })
        })
        .collect()
}

/// Puts `ps` and `inspect` together: by name, compose containers first by project. `inspect` may be missing
/// (it failed, or the tool is another one): the page then has less to say, not nothing.
pub fn merge(ps: &[PsEntry], inspect: &[InspectEntry]) -> Vec<Container> {
    let inspect: HashMap<&str, &InspectEntry> = inspect.iter().map(|entry| (entry.id.as_str(), entry)).collect();
    let mut containers: Vec<Container> = ps
        .iter()
        .map(|entry| {
            let extra = inspect.get(entry.id.as_str()).copied();
            let labels = extra.map(|extra| &extra.labels).filter(|labels| !labels.is_empty()).unwrap_or(&entry.labels);
            let label = |key: &str| labels.get(key).cloned().filter(|value| !value.is_empty());
            Container {
                id: entry.id.clone(),
                name: entry.name.clone(),
                image: entry.image.clone(),
                state: ContainerState::parse(&entry.state),
                status: entry.status.clone(),
                ports: entry.ports.clone(),
                project: label("com.docker.compose.project"),
                config_files: label("com.docker.compose.project.config_files"),
                service: label("com.docker.compose.service"),
                restart_count: extra.map_or(0, |extra| extra.restarts),
                oom_killed: extra.is_some_and(|extra| extra.oom_killed),
                restarting: extra.is_some_and(|extra| extra.restarting),
                memory_limit: extra.map(|extra| extra.memory).filter(|memory| *memory > 0),
                networks: extra.map(|extra| extra.networks.clone()).unwrap_or_default(),
                up_secs: status_age_secs(&entry.status),
                cpu_percent: None,
                mem_used: None,
            }
        })
        .collect();
    containers.sort_by(|a, b| a.project.cmp(&b.project).then_with(|| a.name.cmp(&b.name)));
    containers
}

/// How long a container has been up, from `Up 3 seconds`, `Up About a minute`, `Up 2 hours (healthy)`.
/// `None` for anything else (`Exited …`, `Created`).
pub fn status_age_secs(status: &str) -> Option<u64> {
    let rest = status.strip_prefix("Up ")?.trim();
    let rest = rest.split(" (").next().unwrap_or(rest);
    let lower = rest.to_ascii_lowercase();
    if lower.starts_with("less than") {
        return Some(0);
    }
    let words: Vec<&str> = lower.split_whitespace().collect();
    let number = |word: &str| match word {
        "a" | "an" => Some(1),
        n => n.parse::<u64>().ok(),
    };
    let (count, unit) = match words.as_slice() {
        ["about", n, unit] | [n, unit] => (number(n)?, *unit),
        _ => return None,
    };
    let unit_secs = match unit.trim_end_matches('s') {
        "second" => 1,
        "minute" => 60,
        "hour" => 3600,
        "day" => 86_400,
        "week" => 7 * 86_400,
        "month" => 30 * 86_400,
        "year" => 365 * 86_400,
        _ => return None,
    };
    Some(count * unit_secs)
}

/// Everything the Docker page shows.
#[derive(Clone, Debug, PartialEq)]
pub struct DockerView {
    /// Which tool the numbers come from.
    pub runtime: crate::probe::RuntimeKind,
    /// The version the probe found.
    pub version: Option<String>,
    /// The containers, no-project ones first, then by project and name.
    pub containers: Vec<Container>,
    /// Images and volumes.
    pub usage: Usage,
    /// The images a clean-up would remove.
    pub dangling: Dangling,
    /// Why the last attempt to ask failed (the containers are then the ones from before, if any).
    pub error: Option<String>,
}

/// Lays the numbers of `stats` over the containers (matched by name).
pub fn overlay_stats(containers: &mut [Container], stats: &HashMap<String, StatsEntry>) {
    for container in containers {
        match stats.get(&container.name) {
            Some(entry) => {
                container.cpu_percent = entry.cpu_percent;
                container.mem_used = entry.mem_used;
            }
            None => {
                container.cpu_percent = None;
                container.mem_used = None;
            }
        }
    }
}

// ---- stats ---------------------------------------------------------------------------------

/// One container's line of `stats`.
#[derive(Clone, Debug, PartialEq)]
pub struct StatsEntry {
    /// The container's name.
    pub name: String,
    /// The container's id (short or full).
    pub id: String,
    /// CPU in percent of one core.
    pub cpu_percent: Option<f64>,
    /// Memory in use in bytes.
    pub mem_used: Option<u64>,
}

/// Removes terminal escape sequences (`ESC [ … letter`): `docker stats` clears the screen before every
/// batch even when nobody is a terminal.
pub fn strip_escapes(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// A line of `stats --format '{{json .}}'`, with or without the screen-clearing codes before it.
pub fn parse_stats_line(line: &str) -> Option<StatsEntry> {
    let line = strip_escapes(line);
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let name = string_of(&value, &["Name", "name"])?;
    let percent = |keys: &[&str]| {
        string_of(&value, keys).and_then(|text| text.trim().trim_end_matches('%').trim().parse::<f64>().ok())
    };
    // `412MiB / 1GiB`: the first part is what is in use.
    let mem_used = string_of(&value, &["MemUsage", "mem_usage"])
        .and_then(|text| text.split('/').next().and_then(parse_size));
    Some(StatsEntry {
        name,
        id: string_of(&value, &["ID", "Id", "Container", "id"]).unwrap_or_default(),
        cpu_percent: percent(&["CPUPerc", "CPU", "cpu_percent"]),
        mem_used,
    })
}

/// `12.5MiB`, `1.944GiB`, `0B`, `1.5kB`, `7.8MB`: a size as docker prints it (decimal units with `kB`, `MB`,
/// `GB`, `TB`; binary with `KiB`, `MiB`, …).
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let factor: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0f64.powi(4),
        _ => return None,
    };
    (number.is_finite() && number >= 0.0).then_some((number * factor) as u64)
}

// ---- logs --------------------------------------------------------------------------------------

/// A line of `logs -t`: `2026-11-16T06:10:56.123456789Z message`. A line without a time (a continuation, a
/// message of the tool) is kept as it is.
pub fn parse_log_line(line: &str) -> crate::services::LogLine {
    use crate::services::{LogLine, iso_time, level_of};
    let line = line.trim_end();
    match line.split_once(' ').and_then(|(stamp, rest)| iso_time(stamp).map(|time| (time, rest))) {
        Some((time, rest)) => LogLine { time, text: rest.to_string(), level: level_of(rest) },
        None => LogLine { time: String::new(), text: line.to_string(), level: level_of(line) },
    }
}

// ---- events ------------------------------------------------------------------------------------

/// Whether a line of `events` is a reason to ask again: a container was created, started, died, restarted,
/// renamed, removed. Exec sessions (every `docker exec` makes several) and attaches change nothing.
pub fn is_container_event(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line.trim()) else { return false };
    let action = string_of(&value, &["Action", "status", "action"]).unwrap_or_default();
    !action.is_empty() && !action.starts_with("exec_") && !matches!(action.as_str(), "attach" | "resize")
}

// ---- disk usage --------------------------------------------------------------------------------

/// What the header says about images and volumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// How many images there are.
    pub images: Option<u32>,
    /// What they take, in bytes.
    pub images_size: Option<u64>,
    /// How many volumes there are.
    pub volumes: Option<u32>,
}

/// `system df --format '{{json .}}'`: one row per kind (`Images`, `Containers`, `Local Volumes`, `Build Cache`).
pub fn parse_system_df(text: &str) -> Usage {
    let mut usage = Usage::default();
    for value in text.lines().filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok()) {
        let count = string_of(&value, &["TotalCount", "Total", "total"]).and_then(|n| n.trim().parse::<u32>().ok());
        let size = string_of(&value, &["Size", "size"]).and_then(|size| parse_size(&size));
        match string_of(&value, &["Type", "type"]).unwrap_or_default().as_str() {
            "Images" => {
                usage.images = count;
                usage.images_size = size;
            }
            "Local Volumes" => usage.volumes = count,
            _ => {}
        }
    }
    usage
}

/// The images nothing refers to (what the clean-up removes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dangling {
    /// How many.
    pub count: u32,
    /// What they take, in bytes.
    pub bytes: u64,
}

/// `images --filter dangling=true --format '{{json .}}'`.
pub fn parse_dangling(text: &str) -> Dangling {
    let mut found = Dangling::default();
    for value in text.lines().filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok()) {
        found.count += 1;
        found.bytes += string_of(&value, &["Size", "size"]).and_then(|size| parse_size(&size)).unwrap_or(0);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = r#"{"Command":"\"nginx -g 'daemon off;'\"","CreatedAt":"2026-11-16 01:10:00 +0000 UTC","ID":"aaaaaaaaaaaa1111","Image":"nginx:alpine","Labels":"com.docker.compose.project=shop,com.docker.compose.project.config_files=/srv/shop/compose.yml,/srv/shop/override.yml,com.docker.compose.service=web,maintainer=NGINX Docker Maintainers","LocalVolumes":"0","Mounts":"","Names":"shop-web-1","Networks":"shop_default","Ports":"0.0.0.0:18080->80/tcp, [::]:18080->80/tcp","RunningFor":"2 hours ago","Size":"0B","State":"running","Status":"Up 2 hours"}
{"Command":"\"docker-entrypoint.s…\"","CreatedAt":"2026-11-16 01:10:00 +0000 UTC","ID":"bbbbbbbbbbbb2222","Image":"redis:alpine","Labels":"com.docker.compose.project=shop,com.docker.compose.service=cache","Names":"shop-cache-1","Ports":"6379/tcp","State":"running","Status":"Up About a minute (healthy)"}
{"ID":"cccccccccccc3333","Image":"alpine","Labels":"","Names":"oomer","Ports":"","State":"restarting","Status":"Restarting (137) 3 seconds ago"}
{"ID":"dddddddddddd4444","Image":"alpine","Names":"old-api","Ports":"","State":"exited","Status":"Exited (0) 2 weeks ago"}
WARNING: something odd
"#;

    const INSPECT: &str = r#"{"id":"aaaaaaaaaaaa1111","name":"/shop-web-1","restarts":0,"oom":false,"restarting":false,"memory":0,"labels":{"com.docker.compose.project":"shop","com.docker.compose.project.config_files":"/srv/shop/compose.yml,/srv/shop/override.yml","com.docker.compose.service":"web"},"nets":{"shop_default":{"IPAddress":"172.18.0.2","Gateway":"172.18.0.1"}}}
{"id":"bbbbbbbbbbbb2222","name":"/shop-cache-1","restarts":0,"oom":false,"restarting":false,"memory":0,"labels":{"com.docker.compose.project":"shop","com.docker.compose.service":"cache"},"nets":{"shop_default":{"IPAddress":"172.18.0.3"}}}
{"id":"cccccccccccc3333","name":"/oomer","restarts":6,"oom":true,"restarting":true,"memory":33554432,"labels":null,"nets":{"bridge":{"IPAddress":""}}}
{"id":"dddddddddddd4444","name":"/old-api","restarts":0,"oom":false,"restarting":false,"memory":0,"labels":{},"nets":{}}
"#;

    #[test]
    fn ps_lines_of_docker() {
        let entries = parse_ps(PS);
        assert_eq!(entries.len(), 4, "a warning line is not a container");
        assert_eq!(
            (entries[0].id.as_str(), entries[0].name.as_str(), entries[0].image.as_str()),
            ("aaaaaaaaaaaa1111", "shop-web-1", "nginx:alpine")
        );
        assert_eq!(
            entries[0].ports,
            vec![PortMapping {
                host_ip: Some("0.0.0.0".into()),
                host_port: Some(18080),
                container_port: 80,
                protocol: "tcp".into()
            }],
            "IPv4 and IPv6 are one port"
        );
        assert_eq!(
            entries[1].ports,
            vec![PortMapping { host_ip: None, host_port: None, container_port: 6379, protocol: "tcp".into() }]
        );
        assert_eq!(entries[0].labels["com.docker.compose.project"], "shop");
        assert_eq!(
            entries[0].labels["com.docker.compose.project.config_files"],
            "/srv/shop/compose.yml,/srv/shop/override.yml",
            "a comma inside a value"
        );
        assert_eq!(entries[0].labels["maintainer"], "NGINX Docker Maintainers");
        assert!(entries[3].ports.is_empty());
    }

    #[test]
    fn ps_lines_of_podman() {
        let text = r#"{"Id":"abcdef","Names":["web"],"Image":"docker.io/library/nginx:latest","State":"running","Status":"Up 5 minutes","Ports":[{"host_ip":"","container_port":80,"host_port":8080,"range":1,"protocol":"tcp"},{"host_ip":"","container_port":443,"host_port":0,"protocol":"tcp"}],"Labels":{"app":"x"}}"#;
        let entries = parse_ps(text);
        assert_eq!(entries[0].name, "web");
        assert_eq!(entries[0].ports.len(), 2);
        assert_eq!(entries[0].ports[0].published_target(), Some(("127.0.0.1".into(), 8080)));
        assert!(!entries[0].ports[1].is_published());
        assert_eq!(entries[0].labels["app"], "x");
    }

    #[test]
    fn ports_in_every_shape() {
        assert_eq!(parse_ports("").len(), 0);
        let ports = parse_ports(
            "127.0.0.1:3000->3000/tcp, 9090/tcp, 0.0.0.0:53->53/udp, 127.0.0.1:8000-8002->8000-8002/tcp, :::8080->80/tcp",
        );
        let labels: Vec<String> = ports.iter().map(PortMapping::label).collect();
        assert_eq!(
            labels,
            [
                "127.0.0.1:3000→3000",
                "9090/tcp",
                "53→53/udp",
                "127.0.0.1:8000→8000",
                "127.0.0.1:8001→8001",
                "127.0.0.1:8002→8002",
                "8080→80"
            ]
        );
        assert_eq!(ports[0].published_target(), Some(("127.0.0.1".into(), 3000)));
        assert_eq!(ports[2].published_target(), Some(("127.0.0.1".into(), 53)), "every address: through the loopback");
        assert_eq!(ports[1].published_target(), None);
        let bound = parse_ports("10.0.0.5:8080->80/tcp");
        assert_eq!(bound[0].published_target(), Some(("10.0.0.5".into(), 8080)), "a specific address is where it is");
        // A range that does not pair up, and garbage, give nothing.
        assert!(parse_ports("0.0.0.0:80-90->80-81/tcp").is_empty());
        assert!(parse_ports("nonsense, ->, x/tcp").is_empty());
    }

    #[test]
    fn inspect_adds_restarts_oom_memory_and_addresses() {
        let entries = parse_inspect(INSPECT);
        assert_eq!(entries.len(), 4);
        assert_eq!(
            (entries[2].restarts, entries[2].oom_killed, entries[2].restarting, entries[2].memory),
            (6, true, true, 33_554_432)
        );
        assert_eq!(
            entries[0].networks,
            vec![ContainerNetwork { name: "shop_default".into(), ip: "172.18.0.2".into() }]
        );
        assert!(entries[2].labels.is_empty(), "null labels");
        assert!(parse_inspect("Error: No such object: x\n").is_empty());
    }

    #[test]
    fn the_containers_are_ps_and_inspect_together() {
        let containers = merge(&parse_ps(PS), &parse_inspect(INSPECT));
        assert_eq!(
            containers.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["old-api", "oomer", "shop-cache-1", "shop-web-1"],
            "no project first, then by project and name"
        );
        let web = containers.iter().find(|c| c.name == "shop-web-1").unwrap();
        assert_eq!((web.project.as_deref(), web.service.as_deref()), (Some("shop"), Some("web")));
        assert_eq!(web.config_files.as_deref(), Some("/srv/shop/compose.yml,/srv/shop/override.yml"));
        assert_eq!(
            (web.state.clone(), web.up_secs, web.ip()),
            (ContainerState::Running, Some(7200), Some("172.18.0.2"))
        );
        assert!(!web.is_restart_looping());
        let oomer = containers.iter().find(|c| c.name == "oomer").unwrap();
        assert!(oomer.is_restart_looping() && oomer.oom_killed);
        assert_eq!(oomer.memory_limit, Some(33_554_432));
        assert_eq!(oomer.project, None);
        assert_eq!(oomer.ip(), None, "an empty address is no address");
        let cache = containers.iter().find(|c| c.name == "shop-cache-1").unwrap();
        assert_eq!(cache.up_secs, Some(60));
        // Without inspect there is less to say, but the list is there.
        let bare = merge(&parse_ps(PS), &[]);
        assert_eq!(bare.len(), 4);
        assert_eq!(
            bare.iter().find(|c| c.name == "shop-web-1").unwrap().project.as_deref(),
            Some("shop"),
            "labels of ps are the fallback"
        );
        assert!(
            bare.iter().find(|c| c.name == "oomer").unwrap().is_restart_looping(),
            "the state of ps says restarting"
        );
    }

    #[test]
    fn a_restart_loop_is_a_recent_one() {
        let mut container =
            merge(&parse_ps(PS), &parse_inspect(INSPECT)).into_iter().find(|c| c.name == "shop-web-1").unwrap();
        container.restart_count = 3;
        container.up_secs = Some(40);
        assert!(container.is_restart_looping(), "restarted three times and up for 40 s");
        container.up_secs = Some(86_400);
        assert!(!container.is_restart_looping(), "restarted a few times a while ago and runs fine since");
        container.restart_count = 1;
        container.up_secs = Some(5);
        assert!(!container.is_restart_looping(), "one restart is not a loop");
    }

    #[test]
    fn how_long_a_container_has_been_up() {
        assert_eq!(status_age_secs("Up 3 seconds"), Some(3));
        assert_eq!(status_age_secs("Up Less than a second"), Some(0));
        assert_eq!(status_age_secs("Up About a minute"), Some(60));
        assert_eq!(status_age_secs("Up 5 minutes (healthy)"), Some(300));
        assert_eq!(status_age_secs("Up About an hour"), Some(3600));
        assert_eq!(status_age_secs("Up 11 hours"), Some(39_600));
        assert_eq!(status_age_secs("Up 6 days"), Some(518_400));
        assert_eq!(status_age_secs("Up 2 weeks"), Some(1_209_600));
        assert_eq!(status_age_secs("Exited (0) 2 weeks ago"), None);
        assert_eq!(status_age_secs("Up soon"), None);
        assert_eq!(status_age_secs(""), None);
    }

    #[test]
    fn sizes_as_docker_prints_them() {
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("12.5MiB"), Some(13_107_200));
        assert_eq!(parse_size("1GiB"), Some(1_073_741_824));
        assert_eq!(parse_size("1.5kB"), Some(1500));
        assert_eq!(parse_size("7.8MB"), Some(7_800_000));
        assert_eq!(parse_size("3.2GB"), Some(3_200_000_000));
        assert_eq!(parse_size(" 512 KiB "), Some(524_288));
        assert_eq!(parse_size("12"), Some(12));
        assert_eq!(parse_size("many"), None);
        assert_eq!(parse_size("5 parsecs"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn stats_lines_with_and_without_the_clear_screen_codes() {
        let line = "\u{1b}[2J\u{1b}[H{\"BlockIO\":\"0B / 0B\",\"CPUPerc\":\"4.80%\",\"Container\":\"shop-web-1\",\"ID\":\"aaaaaaaaaaaa\",\"MemPerc\":\"1.20%\",\"MemUsage\":\"412MiB / 1GiB\",\"Name\":\"shop-web-1\",\"NetIO\":\"1kB / 2kB\",\"PIDs\":\"3\"}";
        let entry = parse_stats_line(line).unwrap();
        assert_eq!(
            (entry.name.as_str(), entry.cpu_percent, entry.mem_used),
            ("shop-web-1", Some(4.8), Some(412 * 1024 * 1024))
        );
        let plain = parse_stats_line(r#"{"CPUPerc":"--","Name":"x","MemUsage":"-- / --"}"#).unwrap();
        assert_eq!((plain.cpu_percent, plain.mem_used), (None, None), "a stopped container has no numbers");
        assert!(parse_stats_line("not json").is_none());
        assert!(parse_stats_line("\u{1b}[2J\u{1b}[H").is_none());
        assert_eq!(strip_escapes("a\u{1b}[31mb\u{1b}[0mc"), "abc");
    }

    #[test]
    fn log_lines_with_the_time_docker_adds() {
        use crate::services::LogLevel;
        let line = parse_log_line("2026-11-16T06:10:56.123456789Z [queue] consuming jobs from redis://cache:6379");
        assert_eq!(
            (line.time.as_str(), line.text.as_str(), line.level),
            ("06:10:56", "[queue] consuming jobs from redis://cache:6379", LogLevel::Plain)
        );
        let line = parse_log_line("2026-11-16T06:10:59Z FATAL ERROR: Reached heap limit");
        assert_eq!((line.time.as_str(), line.level), ("06:10:59", LogLevel::Error));
        let line = parse_log_line("no time here");
        assert_eq!((line.time.as_str(), line.text.as_str()), ("", "no time here"));
        assert_eq!(parse_log_line("2026-11-16T06:10:59Z").time, "", "a time and nothing else is not a message");
    }

    #[test]
    fn events_that_change_something() {
        assert!(is_container_event(
            r#"{"status":"die","id":"x","Type":"container","Action":"die","Actor":{"ID":"x","Attributes":{"exitCode":"137"}}}"#
        ));
        assert!(is_container_event(r#"{"status":"start","Action":"start"}"#));
        assert!(!is_container_event(r#"{"status":"exec_start: sh","Action":"exec_start: sh -c ls"}"#), "a docker exec");
        assert!(!is_container_event(r#"{"Action":"exec_die"}"#));
        assert!(!is_container_event(r#"{"Action":"attach"}"#));
        assert!(!is_container_event("Error response from daemon"));
        assert!(!is_container_event("{}"));
    }

    #[test]
    fn disk_usage_and_dangling_images() {
        let df = r#"{"Active":"2","Reclaimable":"1.2GB (93%)","Size":"1.33GB","TotalCount":"14","Type":"Images"}
{"Active":"1","Reclaimable":"0B (0%)","Size":"1.2kB","TotalCount":"6","Type":"Containers"}
{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"3","Type":"Local Volumes"}
{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"0","Type":"Build Cache"}
"#;
        assert_eq!(parse_system_df(df), Usage { images: Some(14), images_size: Some(1_330_000_000), volumes: Some(3) });
        assert_eq!(parse_system_df(""), Usage::default());
        let images = "{\"ID\":\"sha256:1\",\"Repository\":\"<none>\",\"Size\":\"7.8MB\",\"Tag\":\"<none>\"}\n{\"ID\":\"sha256:2\",\"Size\":\"1.2GB\"}\n";
        assert_eq!(parse_dangling(images), Dangling { count: 2, bytes: 1_207_800_000 });
        assert_eq!(parse_dangling(""), Dangling::default());
    }

    #[test]
    fn the_commands() {
        assert_eq!(ps("docker").line(), "docker ps -a --no-trunc --format '{{json .}}'");
        assert_eq!(stats("podman").line(), "podman stats --format '{{json .}}'");
        assert_eq!(events("docker").line(), "docker events --format '{{json .}}' --filter type=container");
        assert_eq!(system_df("docker").line(), "docker system df --format '{{json .}}'");
        assert_eq!(dangling_images("docker").line(), "docker images --filter dangling=true --format '{{json .}}'");
        let inspect = inspect("docker", &["a1".to_string(), "b2".to_string()]).unwrap().line();
        assert!(inspect.starts_with("docker inspect --format '{\"id\":{{json .Id}}"), "{inspect}");
        assert!(inspect.ends_with(" a1 b2"), "{inspect}");
        assert_eq!(super::inspect("docker", &["-f".to_string()]), Err(BuildError::LooksLikeOption));
        assert_eq!(
            exec_shell("docker", "web 1").unwrap().line(),
            "docker exec -it 'web 1' sh -c 'command -v bash >/dev/null && exec bash || exec sh'"
        );
        assert_eq!(logs("docker", "web", true).unwrap().line(), "docker logs -f --tail 100 -t web");
        assert_eq!(logs("docker", "web", false).unwrap().line(), "docker logs --tail 100 -t web");
        assert_eq!(logs("docker", "-x", true), Err(BuildError::LooksLikeOption));
        assert_eq!(exec_shell("docker", ""), Err(BuildError::Empty));
    }

    /// A shell reads the exec line back as the words it was built from, whatever the container is called.
    #[test]
    fn the_exec_line_survives_a_shell() {
        let command = exec_shell("docker", "it's a $name; x").unwrap();
        let script = format!(
            "f() {{ for a in \"$@\"; do printf '%s\\0' \"$a\"; done; }}; f {}",
            &command.line()["docker".len()..]
        );
        let output = std::process::Command::new("sh").arg("-c").arg(script).output().unwrap();
        let words: Vec<String> = output
            .stdout
            .split(|b| *b == 0)
            .filter(|w| !w.is_empty())
            .map(|w| String::from_utf8_lossy(w).into_owned())
            .collect();
        assert_eq!(
            words,
            ["exec", "-it", "it's a $name; x", "sh", "-c", "command -v bash >/dev/null && exec bash || exec sh"]
        );
    }
}
