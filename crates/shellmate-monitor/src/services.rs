//! systemd services: the commands that list them and the parsers of what `systemctl` prints.
//!
//! Three calls make the list, and all of them are read-only (no root needed):
//!
//! * `systemctl list-units --type=service --all --no-legend --plain --no-pager`: the units systemd has in
//!   memory, with their state and description;
//! * `systemctl list-unit-files --type=service --no-legend --no-pager`: every unit file with its enabled
//!   state (and, on the units systemd forgot about because they are inactive, the only trace of them);
//! * one `systemctl show -p Id,MainPID,MemoryCurrent,… unit unit …` for what the first two do not say.
//!
//! The parsers take what the servers print as it is and are written for the differences between systemd
//! versions: `[not set]`, `infinity` and `18446744073709551615` all mean "no number", a property an old
//! systemd does not know is simply missing, the third column of `list-unit-files` (the preset, since
//! systemd 253) is ignored, and the time a unit entered its state is read from the monotonic property
//! (`--timestamp=unix` does not exist everywhere) with the text of the plain timestamp as the fallback.

use std::collections::HashMap;

use crate::commands::{BuildError, Command};

pub mod launchd;

/// How many units one `systemctl show` call is asked about (the command line stays short).
pub const SHOW_BATCH: usize = 100;

/// The properties `systemctl show` is asked for.
pub const SHOW_PROPERTIES: &str = "Id,Description,LoadState,ActiveState,SubState,UnitFileState,MainPID,MemoryCurrent,CPUUsageNSec,StateChangeTimestamp,StateChangeTimestampMonotonic,ActiveEnterTimestamp,ActiveEnterTimestampMonotonic,InactiveEnterTimestamp,InactiveEnterTimestampMonotonic,Result,ExecMainStatus";

/// `systemctl list-units --type=service --all --no-legend --plain --no-pager`.
pub fn list_units() -> Command {
    Command::new(["systemctl", "list-units", "--type=service", "--all", "--no-legend", "--plain", "--no-pager"])
        .expect("a fixed command")
}

/// `systemctl list-unit-files --type=service --no-legend --no-pager`.
pub fn list_unit_files() -> Command {
    Command::new(["systemctl", "list-unit-files", "--type=service", "--no-legend", "--no-pager"])
        .expect("a fixed command")
}

/// `systemctl show -p Id,… a.service b.service`: one block of properties per unit, in the order asked.
pub fn show(units: &[String]) -> Result<Command, BuildError> {
    let mut argv =
        vec!["systemctl".to_string(), "show".into(), "--no-pager".into(), "-p".into(), SHOW_PROPERTIES.into()];
    for unit in units {
        if unit.is_empty() {
            return Err(BuildError::Empty);
        }
        if unit.starts_with('-') {
            return Err(BuildError::LooksLikeOption);
        }
        argv.push(unit.clone());
    }
    Command::new(argv)
}

/// What `journalctl` is run as to see why a unit failed: its last five lines of priority `err` or worse,
/// as plain messages (`-n 5 -p err -o cat`). See [`pick_error_line`].
pub fn journal_last_error(unit: &str) -> Result<Command, BuildError> {
    if unit.is_empty() {
        return Err(BuildError::Empty);
    }
    if unit.starts_with('-') {
        return Err(BuildError::LooksLikeOption);
    }
    Command::new(["journalctl", "-u", unit, "-n", "5", "-p", "err", "-o", "cat", "--no-pager", "-q"])
}

/// The line that says why a unit failed: the last of what [`journal_last_error`] printed that is not
/// systemd's own verdict (`Failed to start …` says nothing the badge does not), or that verdict if it is all
/// there is. At most 300 characters.
pub fn pick_error_line(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    let verdict = |line: &&str| line.starts_with("Failed to start ") || line.contains("Failed with result");
    lines.iter().rev().find(|line| !verdict(line)).or(lines.last()).map(|line| line.chars().take(300).collect())
}

// ---- what systemctl prints -------------------------------------------------------------------

/// A row of `list-units`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitRow {
    /// `nginx.service`
    pub name: String,
    /// `loaded`, `not-found`, `masked`, …
    pub load: String,
    /// `active`, `inactive`, `failed`, `activating`, …
    pub active: String,
    /// `running`, `dead`, `exited`, `failed`, …
    pub sub: String,
    /// The description (the words of the line after the fourth, joined by single spaces).
    pub description: String,
}

/// The rows of `systemctl list-units --type=service --all --no-legend --plain`. Lines that are not a unit
/// (a legend an old version prints anyway, a footer like `123 loaded units listed.`) are skipped; the
/// bullet some versions put before units that need attention is dropped.
pub fn parse_list_units(text: &str) -> Vec<UnitRow> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '●' | '○' | '×' | '*' | '↻'));
            let mut words = line.split_whitespace();
            let name = words.next()?;
            if !is_service_name(name) {
                return None;
            }
            let (load, active, sub) = (words.next()?, words.next()?, words.next()?);
            Some(UnitRow {
                name: name.to_string(),
                load: load.to_string(),
                active: active.to_string(),
                sub: sub.to_string(),
                description: words.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

/// The unit files of `systemctl list-unit-files --type=service --no-legend`: name and state (`enabled`,
/// `disabled`, `static`, `masked`, …). A third column (the preset) is ignored.
pub fn parse_unit_files(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?;
            let state = words.next()?;
            is_service_name(name).then(|| (name.to_string(), state.to_string()))
        })
        .collect()
}

fn is_service_name(name: &str) -> bool {
    name.len() > ".service".len() && name.ends_with(".service") && !name.contains(char::is_control)
}

/// A template (`getty@.service`) is not a service that can be started: only its instances are.
fn is_template(name: &str) -> bool {
    name.strip_suffix(".service").is_some_and(|stem| stem.ends_with('@'))
}

/// The properties of one unit, from `systemctl show`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitProps {
    /// `Id`
    pub name: String,
    /// `Description`
    pub description: Option<String>,
    /// `LoadState`
    pub load: Option<String>,
    /// `ActiveState`
    pub active: Option<String>,
    /// `SubState`
    pub sub: Option<String>,
    /// `UnitFileState`
    pub unit_file_state: Option<String>,
    /// `MainPID`, if there is a main process.
    pub main_pid: Option<u32>,
    /// `MemoryCurrent` in bytes, if memory is accounted.
    pub memory: Option<u64>,
    /// `CPUUsageNSec`: CPU time used, in nanoseconds, if CPU is accounted.
    pub cpu_nsec: Option<u64>,
    /// When the unit entered its current state, in microseconds of the server's monotonic clock.
    pub since_monotonic_us: Option<u64>,
    /// The same as text (`2026-11-16 01:09:15 UTC`), when the server printed one.
    pub since_text: Option<String>,
    /// `Result`: `success`, `exit-code`, `signal`, `timeout`, …
    pub result: Option<String>,
    /// `ExecMainStatus`: the exit status of the main process.
    pub exit_status: Option<i32>,
}

/// A number that systemd prints when it means "unknown".
fn number(value: &str) -> Option<u64> {
    let value = value.trim();
    match value {
        "" | "[not set]" | "infinity" | "n/a" => None,
        _ => value.parse::<u64>().ok().filter(|n| *n != u64::MAX),
    }
}

fn text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value != "[not set]" && value != "n/a").then(|| value.to_string())
}

/// `Mon 2026-11-16 01:09:15 UTC` → `2026-11-16 01:09:15 UTC`: the weekday adds nothing.
fn timestamp_text(value: &str) -> Option<String> {
    let value = text(value)?;
    let mut words = value.split_whitespace();
    let first = words.next()?;
    if first.len() == 3 && first.chars().all(|c| c.is_ascii_alphabetic()) {
        let rest = words.collect::<Vec<_>>().join(" ");
        return (!rest.is_empty()).then_some(rest);
    }
    Some(value)
}

/// The blocks of `systemctl show` (one per unit, separated by blank lines). A property an old systemd does
/// not know is missing, one that is unset says `[not set]`; both give `None`.
pub fn parse_show(output: &str) -> Vec<UnitProps> {
    let mut all = Vec::new();
    let mut block: HashMap<&str, &str> = HashMap::new();
    let mut flush = |block: &mut HashMap<&str, &str>| {
        if let Some(props) = props_of(block) {
            all.push(props);
        }
        block.clear();
    };
    for line in output.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            flush(&mut block);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        // A block without a blank line before the next `Id=` (a version that leaves it out).
        if key == "Id" && block.contains_key("Id") {
            flush(&mut block);
        }
        block.insert(key, value);
    }
    flush(&mut block);
    all
}

fn props_of(block: &HashMap<&str, &str>) -> Option<UnitProps> {
    let get = |key: &str| block.get(key).copied();
    let name = get("Id").map(str::trim).filter(|name| !name.is_empty())?.to_string();
    let main_pid = get("MainPID").and_then(number).and_then(|pid| u32::try_from(pid).ok()).filter(|pid| *pid > 0);
    // When the unit entered the state it is in: the state change if systemd knows it, else the moment the
    // unit became active or inactive.
    let (monotonic, wall) = ["StateChange", "ActiveEnter", "InactiveEnter"]
        .into_iter()
        .map(|prefix| {
            (
                get(&format!("{prefix}TimestampMonotonic")).and_then(number).filter(|us| *us > 0),
                get(&format!("{prefix}Timestamp")).and_then(timestamp_text),
            )
        })
        .find(|(monotonic, wall)| monotonic.is_some() || wall.is_some())
        .unwrap_or((None, None));
    Some(UnitProps {
        name,
        description: get("Description").and_then(text),
        load: get("LoadState").and_then(text),
        active: get("ActiveState").and_then(text),
        sub: get("SubState").and_then(text),
        unit_file_state: get("UnitFileState").and_then(text),
        main_pid,
        memory: get("MemoryCurrent").and_then(number),
        cpu_nsec: get("CPUUsageNSec").and_then(number),
        since_monotonic_us: monotonic,
        since_text: wall,
        result: get("Result").and_then(text).filter(|result| result != "success"),
        exit_status: get("ExecMainStatus").and_then(|v| v.trim().parse::<i32>().ok()),
    })
}

// ---- the model -------------------------------------------------------------------------------

/// Where a unit stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActiveState {
    /// Running (or done, for a one-shot service).
    Active,
    /// Reloading its configuration.
    Reloading,
    /// Starting.
    Activating,
    /// Stopping.
    Deactivating,
    /// Not running.
    Inactive,
    /// A stopped launchd job whose last invocation exited unsuccessfully.
    LastRunFailed,
    /// It ended badly.
    Failed,
    /// Anything else systemd says.
    Other(String),
}

impl ActiveState {
    /// Reads `ActiveState`.
    pub fn parse(word: &str) -> Self {
        match word {
            "active" => ActiveState::Active,
            "reloading" => ActiveState::Reloading,
            "activating" => ActiveState::Activating,
            "deactivating" => ActiveState::Deactivating,
            "inactive" => ActiveState::Inactive,
            "failed" => ActiveState::Failed,
            other => ActiveState::Other(other.to_string()),
        }
    }

    /// The service is up, or on its way up.
    pub fn is_running(&self) -> bool {
        matches!(self, ActiveState::Active | ActiveState::Reloading | ActiveState::Activating)
    }
}

/// Whether a unit starts at boot, and whether that can be changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autostart {
    /// No launch-at-login rule or disabled override.
    Manual,
    /// `enabled` (also `enabled-runtime`).
    Enabled,
    /// `disabled` (also `linked`).
    Disabled,
    /// `static`, `indirect`, `generated`, `alias`, `transient`: started by something else, nothing to switch.
    Fixed,
    /// `masked`: cannot be started at all.
    Masked,
    /// Nobody said.
    Unknown,
}

impl Autostart {
    /// Reads a unit file state.
    pub fn parse(state: &str) -> Self {
        match state {
            "enabled" | "enabled-runtime" => Autostart::Enabled,
            "disabled" | "linked" | "linked-runtime" => Autostart::Disabled,
            "static" | "indirect" | "generated" | "alias" | "transient" => Autostart::Fixed,
            "masked" | "masked-runtime" => Autostart::Masked,
            _ => Autostart::Unknown,
        }
    }

    /// Whether the switch on the page can be used.
    pub fn can_toggle(self) -> bool {
        matches!(self, Autostart::Enabled | Autostart::Disabled)
    }
}

/// One service, as the page shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    /// Native macOS metadata; absent on systemd.
    pub launchd: Option<launchd::Job>,
    /// `nginx.service`
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Where it stands.
    pub active: ActiveState,
    /// `running`, `dead`, `exited`, `failed`, …
    pub sub: String,
    /// `loaded`, `masked`, …; empty if systemd did not say.
    pub load: String,
    /// Starts at boot?
    pub autostart: Autostart,
    /// The main process.
    pub main_pid: Option<u32>,
    /// Memory in use, in bytes.
    pub memory: Option<u64>,
    /// CPU time used, in nanoseconds.
    pub cpu_nsec: Option<u64>,
    /// Since when it is in this state, in microseconds of the server's monotonic clock.
    pub since_monotonic_us: Option<u64>,
    /// Since when, as the server wrote it.
    pub since_text: Option<String>,
    /// `Result` of a unit that ended badly (`exit-code`, `timeout`, …).
    pub result: Option<String>,
    /// The exit status of the main process.
    pub exit_status: Option<i32>,
    /// The last error line of the journal, for a failed unit (when the journal could be read).
    pub last_error: Option<String>,
}

impl Service {
    /// Stable, domain-qualified selection and action identity on macOS.
    pub fn key(&self) -> &str {
        self.launchd.as_ref().map_or(&self.name, |job| &job.target)
    }

    /// Whether this row is hidden by the default local filter.
    pub fn apple(&self) -> bool {
        self.launchd.as_ref().is_some_and(launchd::Job::apple)
    }

    /// The service ended badly.
    pub fn is_failed(&self) -> bool {
        self.active == ActiveState::Failed
    }
}

/// The services of a server.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServiceList {
    /// Failed ones first, then by name.
    pub services: Vec<Service>,
}

impl ServiceList {
    /// How many are failed.
    pub fn failed(&self) -> usize {
        self.services.iter().filter(|service| service.is_failed()).count()
    }

    /// How many are running.
    pub fn running(&self) -> usize {
        self.services.iter().filter(|service| service.active.is_running()).count()
    }

    /// The service called `name`.
    pub fn get(&self, name: &str) -> Option<&Service> {
        self.services.iter().find(|service| service.key() == name)
    }
}

/// Puts the three answers together. Units that systemd does not know (`not-found`: something only referred
/// to them) are left out; unit files whose unit is not in memory (inactive units are forgotten) are
/// listed as inactive, so that they can be started again; templates are not services.
pub fn merge(rows: &[UnitRow], files: &[(String, String)], props: &[UnitProps]) -> Vec<Service> {
    let files: HashMap<&str, &str> = files.iter().map(|(name, state)| (name.as_str(), state.as_str())).collect();
    let props: HashMap<&str, &UnitProps> = props.iter().map(|p| (p.name.as_str(), p)).collect();

    let mut services: HashMap<String, Service> = HashMap::new();
    for row in rows.iter().filter(|row| row.load != "not-found" && !is_template(&row.name)) {
        services.insert(row.name.clone(), blank(&row.name, &row.description, &row.load, &row.active, &row.sub));
    }
    for (name, state) in files.iter().map(|(name, state)| (*name, *state)) {
        if is_template(name) || services.contains_key(name) {
            continue;
        }
        let load = if state.starts_with("masked") { "masked" } else { "" };
        services.insert(name.to_string(), blank(name, "", load, "inactive", "dead"));
    }
    for service in services.values_mut() {
        let file = files.get(service.name.as_str()).copied();
        let props = props.get(service.name.as_str()).copied();
        if let Some(props) = props {
            if let Some(description) = props.description.as_ref().filter(|_| service.description.is_empty()) {
                service.description = description.clone();
            }
            // What `show` says is newer than the list: the list may be older by a few hundred milliseconds.
            if let Some(active) = &props.active {
                service.active = ActiveState::parse(active);
            }
            if let Some(sub) = &props.sub {
                service.sub = sub.clone();
            }
            if let Some(load) = &props.load {
                service.load = load.clone();
            }
            service.main_pid = props.main_pid;
            service.memory = props.memory;
            service.cpu_nsec = props.cpu_nsec;
            service.since_monotonic_us = props.since_monotonic_us;
            service.since_text = props.since_text.clone();
            service.result = props.result.clone();
            service.exit_status = props.exit_status;
        }
        let state = file.or_else(|| props.and_then(|p| p.unit_file_state.as_deref()));
        service.autostart = state.map_or(Autostart::Unknown, Autostart::parse);
        if service.load == "masked" {
            service.autostart = Autostart::Masked;
        }
    }
    let mut services: Vec<Service> = services.into_values().collect();
    services.sort_by(|a, b| b.is_failed().cmp(&a.is_failed()).then_with(|| a.name.cmp(&b.name)));
    services
}

fn blank(name: &str, description: &str, load: &str, active: &str, sub: &str) -> Service {
    Service {
        launchd: None,
        name: name.to_string(),
        description: description.to_string(),
        active: ActiveState::parse(active),
        sub: sub.to_string(),
        load: load.to_string(),
        autostart: Autostart::Unknown,
        main_pid: None,
        memory: None,
        cpu_nsec: None,
        since_monotonic_us: None,
        since_text: None,
        result: None,
        exit_status: None,
        last_error: None,
    }
}

/// The names to ask `systemctl show` about, in batches of [`SHOW_BATCH`]: running and failed units first
/// (those are the ones with numbers), at most `limit` in all.
pub fn names_to_show(rows: &[UnitRow], files: &[(String, String)], limit: usize) -> Vec<String> {
    let mut names: Vec<(bool, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in rows.iter().filter(|row| row.load != "not-found" && !is_template(&row.name)) {
        if seen.insert(row.name.as_str()) {
            names.push((row.active == "inactive", row.name.clone()));
        }
    }
    for (name, _) in files.iter().filter(|(name, _)| !is_template(name)) {
        if seen.insert(name.as_str()) {
            names.push((true, name.clone()));
        }
    }
    names.sort_by_key(|(inactive, _)| *inactive);
    names.into_iter().map(|(_, name)| name).take(limit).collect()
}

// ---- journal lines ---------------------------------------------------------------------------

/// How a line of the journal looks on the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    /// Nothing special.
    Plain,
    /// An error (red).
    Error,
    /// A warning (orange).
    Warning,
    /// Something started or reloaded (green).
    Good,
}

/// A line of `journalctl -o short-iso`, taken apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    /// `09:49:12`; empty if the line has no time (a continuation line, a hint).
    pub time: String,
    /// The message, without the host name and the `unit[pid]:` in front of it.
    pub text: String,
    /// What it looks like.
    pub level: LogLevel,
}

/// Reads a line of `journalctl -o short-iso` (`2026-11-16T09:49:12+0000 host unit[123]: message`). Lines in
/// another shape (`-- Logs begin at …`, `-- No entries --`) are kept as they are, without a time.
pub fn parse_journal_line(line: &str) -> LogLine {
    let line = line.trim_end();
    let mut parts = line.splitn(3, ' ');
    let (stamp, host, rest) = (parts.next().unwrap_or(""), parts.next(), parts.next());
    let time = iso_time(stamp);
    let message = match (time.as_ref(), host, rest) {
        (Some(_), Some(_), Some(rest)) => {
            // `unit[123]: message`; the program name has no spaces.
            match rest.split_once(": ") {
                Some((ident, message)) if !ident.contains(' ') => message,
                _ => rest,
            }
        }
        _ => line,
    };
    LogLine { time: time.unwrap_or_default(), text: message.to_string(), level: level_of(message) }
}

/// `2026-11-16T09:49:12+0000` → `09:49:12`.
pub(crate) fn iso_time(stamp: &str) -> Option<String> {
    let (date, time) = stamp.split_once('T')?;
    if date.len() != 10
        || !date.bytes().enumerate().all(|(i, b)| if matches!(i, 4 | 7) { b == b'-' } else { b.is_ascii_digit() })
    {
        return None;
    }
    let clock: String = time.chars().take(8).collect();
    let valid = clock.len() == 8
        && clock.bytes().enumerate().all(|(i, b)| if matches!(i, 2 | 5) { b == b':' } else { b.is_ascii_digit() });
    valid.then_some(clock)
}

/// What a message of a log looks like at a glance.
pub fn level_of(message: &str) -> LogLevel {
    let lower = message.to_ascii_lowercase();
    if ["error", "fail", "fatal", "panic", "exception", "critical", "denied"].iter().any(|word| lower.contains(word)) {
        LogLevel::Error
    } else if lower.contains("warn") {
        LogLevel::Warning
    } else if message.starts_with("Started ") || message.starts_with("Reloaded ") || message.starts_with("Finished ") {
        LogLevel::Good
    } else {
        LogLevel::Plain
    }
}

/// The line `journalctl` prints when the user may not read the system journal: no error, no exit status,
/// only this hint (and then nothing).
pub fn is_journal_hint(line: &str) -> bool {
    line.contains("not seeing messages from other users and the system")
        || line.contains("can see all messages")
        || line.contains("Pass -q to turn off this notice")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// systemd 255 (Ubuntu 24.04): `list-units --plain --no-legend`.
    const LIST_UNITS_255: &str = "\
apparmor.service                             loaded    inactive dead    Load AppArmor profiles
auditd.service                               masked    inactive dead    auditd.service
connman.service                              not-found inactive dead    connman.service
cron.service                                 loaded    active   running Regular background program processing daemon
getty@tty1.service                           loaded    active   running Getty on tty1
shellmate-broken.service                     loaded    failed   failed  Shellmate test (broken)
ssh.service                                  loaded    active   running OpenBSD Secure Shell server
systemd-journald.service                     loaded    active   running Journal Service
";

    /// An older systemd (219, CentOS 7): the bullet marks the failed unit even with `--plain`, and descriptions
    /// have spaces.
    const LIST_UNITS_OLD: &str = "\
  auditd.service                loaded active running Security Auditing Service
● kdump.service                 loaded failed failed  Crash recovery kernel arming
  NetworkManager.service        loaded active running Network Manager
  rhel-dmesg.service            loaded active exited  Dump dmesg to /var/log/dmesg
";

    const UNIT_FILES_255: &str = "\
apparmor.service                               enabled         enabled
auditd.service                                 masked          enabled
cron.service                                   enabled         enabled
getty@.service                                 enabled         enabled
shellmate-broken.service                       disabled        enabled
shellmate-demo.service                         disabled        enabled
ssh.service                                    enabled         enabled
systemd-journald.service                       static          -
systemd-timesyncd.service                      enabled         enabled
";

    const UNIT_FILES_OLD: &str = "\
auditd.service                                enabled
kdump.service                                 enabled
NetworkManager.service                        enabled
rhel-dmesg.service                            enabled
";

    const SHOW_255: &str = "\
Id=cron.service
Description=Regular background program processing daemon
LoadState=loaded
ActiveState=active
SubState=running
UnitFileState=enabled
MainPID=640
MemoryCurrent=2203648
CPUUsageNSec=1234567890
StateChangeTimestamp=Mon 2026-11-16 01:09:15 UTC
StateChangeTimestampMonotonic=12345678
ActiveEnterTimestamp=Mon 2026-11-16 01:09:15 UTC
ActiveEnterTimestampMonotonic=12345678
InactiveEnterTimestamp=n/a
InactiveEnterTimestampMonotonic=0
Result=success
ExecMainStatus=0

Id=shellmate-broken.service
Description=Shellmate test (broken)
LoadState=loaded
ActiveState=failed
SubState=failed
UnitFileState=disabled
MainPID=0
MemoryCurrent=[not set]
CPUUsageNSec=[not set]
StateChangeTimestamp=Mon 2026-11-16 01:10:00 UTC
StateChangeTimestampMonotonic=57000000
ActiveEnterTimestamp=n/a
ActiveEnterTimestampMonotonic=0
InactiveEnterTimestamp=Mon 2026-11-16 01:10:00 UTC
InactiveEnterTimestampMonotonic=57000000
Result=exit-code
ExecMainStatus=1

Id=apparmor.service
Description=Load AppArmor profiles
LoadState=loaded
ActiveState=inactive
SubState=dead
UnitFileState=enabled
MainPID=0
MemoryCurrent=18446744073709551615
CPUUsageNSec=18446744073709551615
StateChangeTimestamp=
StateChangeTimestampMonotonic=0
ActiveEnterTimestamp=
ActiveEnterTimestampMonotonic=0
Result=success
ExecMainStatus=0
";

    /// An old systemd: no CPUUsageNSec, no StateChangeTimestamp, no MemoryCurrent for the unit that is not
    /// accounted, `infinity` in a place a number goes, and no blank lines between the blocks.
    const SHOW_OLD: &str = "\
Id=kdump.service
Description=Crash recovery kernel arming
LoadState=loaded
ActiveState=failed
SubState=failed
MainPID=0
MemoryCurrent=infinity
ActiveEnterTimestamp=Tue 2020-01-07 10:00:01 CET
Result=exit-code
ExecMainStatus=1
Id=auditd.service
Description=Security Auditing Service
LoadState=loaded
ActiveState=active
SubState=running
MainPID=612
ActiveEnterTimestamp=Tue 2020-01-07 09:00:00 CET
ActiveEnterTimestampMonotonic=8000000
Result=success
";

    #[test]
    fn list_units_of_systemd_255() {
        let rows = parse_list_units(LIST_UNITS_255);
        assert_eq!(rows.len(), 8);
        assert_eq!(
            rows[3],
            UnitRow {
                name: "cron.service".into(),
                load: "loaded".into(),
                active: "active".into(),
                sub: "running".into(),
                description: "Regular background program processing daemon".into()
            }
        );
        assert_eq!(rows[5].active, "failed");
        assert_eq!(rows[2].load, "not-found");
    }

    #[test]
    fn list_units_of_an_old_systemd_with_bullets_and_a_footer() {
        let text = format!(
            "{LIST_UNITS_OLD}\nLOAD   = Reflects whether the unit definition was properly loaded.\n\n4 loaded units listed.\n"
        );
        let rows = parse_list_units(&text);
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["auditd.service", "kdump.service", "NetworkManager.service", "rhel-dmesg.service"]
        );
        assert_eq!((rows[1].active.as_str(), rows[1].sub.as_str()), ("failed", "failed"));
        assert_eq!(rows[3].description, "Dump dmesg to /var/log/dmesg");
    }

    #[test]
    fn units_that_are_not_services_or_not_whole_lines_are_skipped() {
        assert!(parse_list_units("sshd.socket loaded active listening OpenBSD\n.service loaded active running x\nshort.service loaded\n\n").is_empty());
        assert!(parse_unit_files("ssh.socket enabled\nlonely.service\n").is_empty());
    }

    #[test]
    fn unit_files_with_and_without_the_preset_column() {
        let new = parse_unit_files(UNIT_FILES_255);
        assert_eq!(new.len(), 9);
        assert!(new.contains(&("shellmate-demo.service".to_string(), "disabled".to_string())));
        assert!(new.contains(&("systemd-journald.service".to_string(), "static".to_string())));
        let old = parse_unit_files(UNIT_FILES_OLD);
        assert_eq!(old[1], ("kdump.service".to_string(), "enabled".to_string()));
    }

    #[test]
    fn show_of_systemd_255() {
        let props = parse_show(SHOW_255);
        assert_eq!(props.len(), 3);
        let cron = &props[0];
        assert_eq!(cron.name, "cron.service");
        assert_eq!((cron.main_pid, cron.memory, cron.cpu_nsec), (Some(640), Some(2_203_648), Some(1_234_567_890)));
        assert_eq!(cron.since_monotonic_us, Some(12_345_678));
        assert_eq!(cron.since_text.as_deref(), Some("2026-11-16 01:09:15 UTC"), "no weekday");
        assert_eq!(cron.result, None, "success is nothing to report");
        let broken = &props[1];
        assert_eq!(
            (broken.main_pid, broken.memory, broken.cpu_nsec),
            (None, None, None),
            "[not set], and a main pid of 0"
        );
        assert_eq!((broken.result.as_deref(), broken.exit_status), (Some("exit-code"), Some(1)));
        assert_eq!(broken.since_monotonic_us, Some(57_000_000), "the state change, not the activation that never was");
        let apparmor = &props[2];
        assert_eq!((apparmor.memory, apparmor.cpu_nsec), (None, None), "u64::MAX is what an old systemd calls unset");
        assert_eq!((apparmor.since_monotonic_us, apparmor.since_text.as_deref()), (None, None));
    }

    #[test]
    fn show_of_an_old_systemd_without_some_properties_and_blank_lines() {
        let props = parse_show(SHOW_OLD);
        assert_eq!(props.len(), 2, "a repeated Id starts the next block");
        let kdump = &props[0];
        assert_eq!((kdump.memory, kdump.cpu_nsec), (None, None), "infinity and a missing property");
        assert_eq!(kdump.since_monotonic_us, None);
        assert_eq!(
            kdump.since_text.as_deref(),
            Some("2020-01-07 10:00:01 CET"),
            "the plain timestamp text is the fallback"
        );
        let auditd = &props[1];
        assert_eq!((auditd.main_pid, auditd.since_monotonic_us), (Some(612), Some(8_000_000)));
    }

    #[test]
    fn show_with_garbage_gives_what_it_can() {
        assert!(parse_show("").is_empty());
        assert!(parse_show("not a property\nMainPID=1\n").is_empty(), "no Id: not a unit");
        let props = parse_show("Id=x.service\nMainPID=abc\nMemoryCurrent=-5\nExecMainStatus=zzz\n");
        assert_eq!(props[0].main_pid, None);
        assert_eq!((props[0].memory, props[0].exit_status), (None, None));
    }

    #[test]
    fn the_three_answers_make_the_list() {
        let rows = parse_list_units(LIST_UNITS_255);
        let files = parse_unit_files(UNIT_FILES_255);
        let props = parse_show(SHOW_255);
        let services = merge(&rows, &files, &props);
        let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names[0], "shellmate-broken.service", "failed units come first");
        assert!(!names.contains(&"connman.service"), "not-found units are not services");
        assert!(!names.contains(&"getty@.service"), "a template is not a service");
        assert!(names.contains(&"getty@tty1.service"), "but its instance is");
        assert!(names.contains(&"shellmate-demo.service"), "a unit file whose unit systemd forgot is still listed");
        let demo = services.iter().find(|s| s.name == "shellmate-demo.service").unwrap();
        assert_eq!(demo.active, ActiveState::Inactive);
        assert_eq!(demo.autostart, Autostart::Disabled);
        let cron = services.iter().find(|s| s.name == "cron.service").unwrap();
        assert_eq!(
            (cron.active.clone(), cron.autostart, cron.main_pid, cron.memory),
            (ActiveState::Active, Autostart::Enabled, Some(640), Some(2_203_648))
        );
        let audit = services.iter().find(|s| s.name == "auditd.service").unwrap();
        assert_eq!(audit.autostart, Autostart::Masked);
        let journald = services.iter().find(|s| s.name == "systemd-journald.service").unwrap();
        assert_eq!(journald.autostart, Autostart::Fixed);
        assert!(!journald.autostart.can_toggle() && cron.autostart.can_toggle());
        let broken = &services[0];
        assert_eq!((broken.result.as_deref(), broken.exit_status), (Some("exit-code"), Some(1)));
        let list = ServiceList { services };
        assert_eq!((list.failed(), list.get("ssh.service").map(|s| s.active.is_running())), (1, Some(true)));
        assert_eq!(list.running(), 4);
    }

    #[test]
    fn a_unit_the_list_does_not_know_gets_its_state_from_show_and_a_missing_file_state_from_show_too() {
        let rows = parse_list_units(LIST_UNITS_OLD);
        let services = merge(&rows, &[], &parse_show(SHOW_OLD));
        let kdump = services.iter().find(|s| s.name == "kdump.service").unwrap();
        assert!(kdump.is_failed());
        assert_eq!(kdump.autostart, Autostart::Unknown, "nobody said");
        let props = parse_show("Id=a.service\nUnitFileState=enabled\nActiveState=active\nSubState=running\n");
        let services = merge(&[], &[], &props);
        assert!(services.is_empty(), "show alone does not make a unit appear");
        let rows = parse_list_units("a.service loaded active running A\n");
        assert_eq!(merge(&rows, &[], &props)[0].autostart, Autostart::Enabled, "UnitFileState of show");
    }

    #[test]
    fn the_names_for_show_put_running_units_first_and_are_limited() {
        let rows = parse_list_units(LIST_UNITS_255);
        let files = parse_unit_files(UNIT_FILES_255);
        let names = names_to_show(&rows, &files, 100);
        assert!(!names.contains(&"getty@.service".to_string()) && !names.contains(&"connman.service".to_string()));
        assert!(names.iter().position(|n| n == "cron.service") < names.iter().position(|n| n == "apparmor.service"));
        assert_eq!(names.len(), names.iter().collect::<std::collections::HashSet<_>>().len(), "once each");
        assert_eq!(names_to_show(&rows, &files, 3).len(), 3);
    }

    #[test]
    fn the_commands() {
        assert_eq!(list_units().line(), "systemctl list-units --type=service --all --no-legend --plain --no-pager");
        assert_eq!(list_unit_files().line(), "systemctl list-unit-files --type=service --no-legend --no-pager");
        let command = show(&["a.service".to_string(), "b c.service".to_string()]).unwrap().line();
        assert!(command.starts_with("systemctl show --no-pager -p Id,Description,"), "{command}");
        assert!(command.ends_with(" a.service 'b c.service'"), "{command}");
        assert_eq!(show(&["--now".to_string()]), Err(BuildError::LooksLikeOption));
        assert_eq!(show(&[String::new()]), Err(BuildError::Empty));
        assert_eq!(
            journal_last_error("nginx.service").unwrap().line(),
            "journalctl -u nginx.service -n 5 -p err -o cat --no-pager -q"
        );
        assert_eq!(journal_last_error("-x"), Err(BuildError::LooksLikeOption));
    }

    #[test]
    fn the_line_that_says_why_a_unit_failed() {
        let out = "ERROR: [pool www] cannot get uid for user 'www-data2'\nFailed to start php8.2-fpm.service - The PHP FastCGI Process Manager.\n";
        assert_eq!(
            pick_error_line(out).as_deref(),
            Some("ERROR: [pool www] cannot get uid for user 'www-data2'"),
            "the program's own words, not systemd's verdict"
        );
        assert_eq!(
            pick_error_line("Failed to start x.service - X.\n").as_deref(),
            Some("Failed to start x.service - X."),
            "the verdict if nothing else"
        );
        assert_eq!(pick_error_line("first\nsecond\n\n").as_deref(), Some("second"));
        assert_eq!(pick_error_line("\n \n"), None);
        assert_eq!(pick_error_line(&"x".repeat(400)).unwrap().len(), 300);
    }

    #[test]
    fn journal_lines_lose_the_date_the_host_and_the_program() {
        let line =
            parse_journal_line("2026-11-16T09:49:12+0000 web-1 php8.2-fpm[1234]: ERROR: [pool www] cannot get uid");
        assert_eq!(
            (line.time.as_str(), line.text.as_str(), line.level),
            ("09:49:12", "ERROR: [pool www] cannot get uid", LogLevel::Error)
        );
        let line =
            parse_journal_line("2026-11-16T09:49:11+01:00 web-1 systemd[1]: Started shellmate-demo.service - Demo.");
        assert_eq!(
            (line.time.as_str(), line.text.as_str(), line.level),
            ("09:49:11", "Started shellmate-demo.service - Demo.", LogLevel::Good)
        );
        let line = parse_journal_line("2026-11-16T09:49:11.123456+0000 h x: a: b");
        assert_eq!(line.time, "09:49:11");
        assert_eq!(line.text, "a: b", "only the first `: ` ends the program name");
        let line = parse_journal_line("-- No entries --");
        assert_eq!((line.time.as_str(), line.text.as_str(), line.level), ("", "-- No entries --", LogLevel::Plain));
        let line = parse_journal_line("2026-11-16T09:49:11+0000 h a message without a program");
        assert_eq!(line.text, "a message without a program");
        assert_eq!(parse_journal_line("2026-13-xx weird").time, "");
        assert_eq!(parse_journal_line("2026-11-16T09:49:11+0000 h ssh[1]: Warning: x").level, LogLevel::Warning);
    }

    #[test]
    fn the_hint_of_a_user_who_may_not_read_the_journal() {
        assert!(is_journal_hint("Hint: You are currently not seeing messages from other users and the system."));
        assert!(is_journal_hint("      Users in groups 'adm', 'systemd-journal' can see all messages."));
        assert!(!is_journal_hint("2026-11-16T09:49:11+0000 h a: hello"));
    }
}
