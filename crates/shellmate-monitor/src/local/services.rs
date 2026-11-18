//! Read-only launchd inventory. Absence means unloaded only after a complete domain listing.

mod logs;
mod plists;
pub(crate) use logs::follow_file;
#[cfg(test)]
mod tests;

use super::runner;
use crate::Snapshot;
use crate::commands::{Command, Plan, ServiceAction};
use crate::monitor::ActionOutcome;
use crate::services::{ActiveState, Autostart, Service, ServiceList, launchd::Job};
use plists::{Cache, Entry, Tails};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) fn validated(
    list: &ServiceList,
    key: &str,
    action: ServiceAction,
    expected: &Command,
) -> Option<(bool, Command)> {
    let service = list.get(key)?;
    let job = service.launchd.as_ref()?;
    let words = job.command(action).ok()?;
    (words == *expected && (action != ServiceAction::Stop || service.active.is_running()))
        .then(|| (job.system(), words))
}

pub(crate) async fn dispatch<F, Fut>(system: bool, words: Command, run: F) -> ActionOutcome
where
    F: FnOnce(tokio::process::Command) -> Fut,
    Fut: Future<Output = std::io::Result<shellmate_ssh::ExecOutput>>,
{
    if system {
        return ActionOutcome::Terminal(Plan::Terminal {
            line: words.prefixed(&["sudo"]).line(),
        });
    }
    let mut cmd = tokio::process::Command::new("/bin/launchctl");
    cmd.args(&words.argv()[1..]);
    crate::monitor::outcome(run(cmd).await.map_err(|err| err.to_string()))
}

#[derive(Default)]
struct Listing {
    rows: HashMap<String, Option<(u32, Option<i32>)>>,
    complete: bool,
}

fn valid_label(label: &str) -> bool {
    !label.is_empty() && !label.starts_with('-') && !label.contains('/') && !label.chars().any(char::is_control)
}

fn parse_row(line: &str, list: &mut Listing) {
    let mut fields = line.split_whitespace();
    let (Some(pid), Some(exit), Some(label), None) = (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        list.complete = false;
        return;
    };
    if !valid_label(label) {
        list.complete = false;
        return;
    }
    let pid = if pid == "-" { Some(0) } else { pid.parse().ok() };
    let exit = if exit == "-" {
        Some(None)
    } else {
        exit.parse().ok().map(Some)
    };
    list.rows.insert(label.to_owned(), pid.zip(exit));
}

fn parse_list(text: &str) -> Listing {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let mut list = Listing::default();
    let Some(header) = lines.next() else { return list };
    list.complete = header.split_whitespace().eq(["PID", "Status", "Label"]);
    if !list.complete {
        parse_row(header, &mut list);
    }
    for line in lines {
        parse_row(line, &mut list);
    }
    list
}

fn block<'a>(text: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let mut lines = text.lines().map(str::trim);
    lines.find(|line| {
        line.split_once('=')
            .is_some_and(|(key, value)| key.trim() == name && value.trim() == "{")
    })?;
    let mut rows = Vec::new();
    for line in lines {
        if line == "}" {
            return Some(rows);
        }
        if line.ends_with('{') {
            return None;
        }
        if !line.is_empty() {
            rows.push(line);
        }
    }
    None
}

fn parse_system(text: &str) -> Listing {
    let Some(rows) = block(text, "services") else {
        return Listing::default();
    };
    let mut list = Listing {
        complete: true,
        ..Listing::default()
    };
    for line in rows {
        parse_row(line, &mut list);
    }
    list
}

fn parse_disabled(text: &str) -> Option<HashMap<String, bool>> {
    block(text, "disabled services")?
        .into_iter()
        .map(|line| {
            let (label, state) = line.split_once("=>")?;
            let label = label.trim().strip_prefix('"')?.strip_suffix('"')?;
            Some((
                label.to_owned(),
                match state.trim() {
                    "disabled" => true,
                    "enabled" => false,
                    _ => return None,
                },
            ))
        })
        .collect()
}

async fn read_cmd(program: &str, args: &[&str]) -> Option<String> {
    let output = runner::output(
        tokio::process::Command::new(program).args(args),
        Duration::from_secs(8),
        4 << 20,
    )
    .await
    .ok()?;
    (output.status.success() && !output.truncated)
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

#[derive(Default)]
pub(crate) struct Inventory {
    cache: Cache,
    tails: Tails,
}

impl Inventory {
    pub(crate) async fn sample(
        &mut self,
        snapshot: Option<Arc<Snapshot>>,
        previous: Option<Arc<ServiceList>>,
    ) -> ServiceList {
        let gui = format!("gui/{}", unsafe { libc::getuid() });
        let roots = plists::roots(std::env::var_os("HOME").map(PathBuf::from));
        let gui_disabled = ["print-disabled", &gui];
        let (user, system, user_overrides, system_overrides, entries) = tokio::join!(
            read_cmd("/bin/launchctl", &["list"]),
            read_cmd("/bin/launchctl", &["print", "system"]),
            read_cmd("/bin/launchctl", &gui_disabled),
            read_cmd("/bin/launchctl", &["print-disabled", "system"]),
            self.cache.scan(roots),
        );
        let mut services = Vec::new();
        for (domain, listing, overrides) in [
            (
                gui.as_str(),
                parse_list(user.as_deref().unwrap_or_default()),
                user_overrides,
            ),
            (
                "system",
                parse_system(system.as_deref().unwrap_or_default()),
                system_overrides,
            ),
        ] {
            let files = entries.iter().filter(|entry| entry.system == (domain == "system"));
            services.extend(merge(
                domain,
                listing,
                parse_disabled(overrides.as_deref().unwrap_or_default()),
                files,
                snapshot.as_deref(),
                previous.as_deref(),
                &mut self.tails,
            ));
        }
        self.tails.finish_round();
        services.sort_by_key(|service| {
            let job = service.launchd.as_ref().unwrap();
            (
                if job.formula().is_some() {
                    0
                } else if job.system() {
                    2
                } else {
                    1
                },
                service.name.clone(),
                job.target.clone(),
            )
        });
        // `application.<bundle id>.<numbers>` jobs are apps LaunchServices started, not services.
        services.retain(|service| !service.launchd.as_ref().is_some_and(|job| job.app_instance()));
        ServiceList { services }
    }
}

fn merge<'a>(
    domain: &str,
    listing: Listing,
    overrides: Option<HashMap<String, bool>>,
    files: impl Iterator<Item = &'a Entry>,
    snapshot: Option<&Snapshot>,
    previous: Option<&ServiceList>,
    tails: &mut Tails,
) -> Vec<Service> {
    let mut entries = BTreeMap::new();
    for entry in files {
        entries.entry(entry.label.as_str()).or_insert(Some(entry));
    }
    for label in listing.rows.keys() {
        entries.entry(label).or_default();
    }
    if !listing.complete
        && let Some(previous) = previous
    {
        for job in previous.services.iter().filter_map(|service| service.launchd.as_ref()) {
            if job.target.rsplit_once('/').is_some_and(|(prefix, _)| prefix == domain) {
                entries.entry(job.label()).or_default();
            }
        }
    }
    entries
        .into_iter()
        .map(|(label, entry)| {
            let row = listing.rows.get(label);
            let plist = entry.and_then(|entry| entry.data.as_ref());
            let loaded = if row.is_some() {
                Some(true)
            } else {
                listing.complete.then_some(false)
            };
            let reading = row.copied().flatten();
            let pid = reading.map(|(pid, _)| pid).filter(|pid| *pid > 0);
            let exit = reading.and_then(|(_, exit)| exit);
            let proc = pid.and_then(|pid| snapshot?.process(pid));
            let bad_plist = entry.is_some() && plist.is_none();
            let (active, sub) = match (loaded, reading, bad_plist) {
                (_, _, true) => (ActiveState::Other("unknown".into()), "状态未知"),
                (Some(false), _, _) => (ActiveState::Inactive, "未加载"),
                (_, Some((1.., _)), _) => (ActiveState::Active, "运行中"),
                (_, Some((0, Some(status))), _) if status != 0 => {
                    if plist.is_some_and(|data| data.should_run) {
                        (ActiveState::Failed, "失败")
                    } else {
                        (ActiveState::LastRunFailed, "上次运行失败")
                    }
                }
                (_, Some((0, _)), _) => (ActiveState::Inactive, "未运行"),
                _ => (ActiveState::Other("unknown".into()), "状态未知"),
            };
            let autostart = match (&overrides, plist) {
                (Some(map), _) if map.get(label) == Some(&true) => Autostart::Disabled,
                (Some(_), Some(data)) if data.run_at_load || data.keep_alive => Autostart::Enabled,
                (Some(_), Some(_)) => Autostart::Manual,
                _ => Autostart::Unknown,
            };
            let log = plist
                .and_then(|data| data.stderr.as_ref().or(data.stdout.as_ref()))
                .cloned();
            let last_error = (active == ActiveState::Failed).then(|| {
                let mut message = format!("上次退出码 {}", exit.unwrap());
                if let Some(data) = plist
                    && let Some(tail) = [&data.stderr, &data.stdout]
                        .into_iter()
                        .flatten()
                        .find_map(|path| tails.read(path))
                {
                    message.push('\n');
                    message.push_str(&tail);
                }
                message
            });
            let job = Job {
                target: format!("{domain}/{label}"),
                plist: entry.map(|entry| entry.path.clone()),
                keep_alive: plist.map(|data| data.keep_alive),
                loaded: if bad_plist || (row.is_some() && reading.is_none()) {
                    None
                } else {
                    loaded
                },
                log,
            };
            Service {
                name: job.formula().unwrap_or(label).into(),
                launchd: Some(job),
                description: plist.and_then(|data| data.program.clone()).unwrap_or_default(),
                sub: sub.into(),
                load: String::new(),
                active,
                autostart,
                main_pid: pid,
                memory: proc.and_then(|proc| proc.rss_bytes),
                cpu_nsec: None,
                since_monotonic_us: None,
                since_text: proc.and_then(|proc| local_time(proc.start_time)),
                result: None,
                exit_status: exit,
                last_error,
            }
        })
        .collect()
}

/// This Mac's own clock, in its time zone: the start of a local service needs no zone label.
fn local_time(seconds: u64) -> Option<String> {
    let seconds = seconds.try_into().ok()?;
    let mut stamp: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut stamp) }.is_null() {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        stamp.tm_year + 1900,
        stamp.tm_mon + 1,
        stamp.tm_mday,
        stamp.tm_hour,
        stamp.tm_min,
        stamp.tm_sec
    ))
}
