//! The tasks that ask about services and containers for as long as the monitor collects, and the follower
//! of a command's output (a log).
//!
//! They live with the run of the monitor (the collector's lifetime): they start when it has probed the
//! server, and end with it (the connection is lost, nobody looks any more, the monitor is shut down). What
//! they find is kept in the monitor's state so that a page opened later has it at once, and reported as
//! [`MonitorEvent`]s when it changed.
//!
//! * **Services** (when the server runs systemd): the three `systemctl` calls of [`crate::services`] every
//!   10 seconds while the Services page is in front, every 30 otherwise (the overview and the header say
//!   how many failed), and at once when asked.
//! * **Containers** (when there is a docker or podman the user can use): `ps` and `inspect` on the same
//!   rhythm, again after a container event (a long-lived `events` stream, always on), and the disk usage
//!   when the Docker page is in front. A second stream (`stats`) runs only while the Docker page is.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use shellmate_ssh::{ExecOptions, ExecOutput, ExecStatus};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use super::*;
use crate::commands::Command;
use crate::docker::{self, Container, StatsEntry};
use crate::probe::{ContainerAccess, ContainerRuntime, JournalAccess};
use crate::services;

/// What `ps`, `inspect`, `system df` and the dangling images last said.
#[derive(Clone, Default)]
pub(super) struct DockerBase {
    containers: Vec<Container>,
    usage: Usage,
    dangling: Dangling,
    error: Option<String>,
}

/// What is known about the containers: the answers of the commands, the numbers of `stats`, and the view
/// made of them.
pub(super) struct DockerState {
    base: DockerBase,
    stats: HashMap<String, StatsEntry>,
    view: Option<Arc<DockerView>>,
}

impl DockerState {
    pub(super) fn view(&self) -> Option<Arc<DockerView>> {
        self.view.clone()
    }
}

/// Changes what is known about the containers and reports the view if it is not what it was.
fn apply_docker(
    inner: &Inner,
    state: &mut State,
    runtime: &ContainerRuntime,
    change: impl FnOnce(&mut DockerBase, &mut HashMap<String, StatsEntry>),
) {
    let docker = state.docker.get_or_insert_with(|| DockerState {
        base: DockerBase::default(),
        stats: HashMap::new(),
        view: None,
    });
    change(&mut docker.base, &mut docker.stats);
    let mut containers = docker.base.containers.clone();
    docker::overlay_stats(&mut containers, &docker.stats);
    let view = Arc::new(DockerView {
        runtime: runtime.kind,
        version: runtime.version.clone(),
        containers,
        usage: docker.base.usage.clone(),
        dangling: docker.base.dangling,
        error: docker.base.error.clone(),
    });
    if docker.view.as_deref() != Some(&*view) {
        docker.view = Some(view.clone());
        inner.emit(state, MonitorEvent::Docker(view));
    }
}

/// How much a command may print: a few hundred containers fit in 4 MiB.
const MAX_OUTPUT: usize = 4 << 20;
/// How long a command may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(25);
/// How many units are asked about in all (the ones that run, then the rest).
const SHOW_LIMIT: usize = 400;
/// How many failed units get their last error line looked up in one round.
const ERRORS_PER_ROUND: usize = 4;

fn section_script(commands: &[String]) -> String {
    let body = commands.iter().map(|command| format!("{command}\nprintf '\\000'\n")).collect::<String>();
    crate::script::sh_command(&body)
}

fn sections(text: &str) -> impl Iterator<Item = &str> {
    text.rsplit_once('\0').into_iter().flat_map(|(complete, _)| complete.split('\0'))
}

fn docker_script(runtime: &ContainerRuntime) -> String {
    let tool = runtime.kind.command();
    let ps = Command::new([tool, "ps", "-a", "--no-trunc", "--format", "{{.ID}}:{{json .}}"]).unwrap();
    let ps = Task::docker_line(runtime, ps);
    let inspect = Task::docker_line(runtime, docker::inspect(tool, &[]).unwrap());
    crate::script::sh_command(&format!(
        r#"{{ {ps} && echo END; }} | (
set --
while IFS= read -r row; do
  if [ "$row" = END ]; then
    if [ "$#" -gt 0 ]; then {inspect} "$@"; fi
    exit 0
  fi
  id=${{row%%:*}}
  printf 'P%s\n' "${{row#*:}}"
  case "$id" in ""|*[!0-9a-fA-F]*) continue;; esac
  set -- "$@" "$id"
  if [ "$#" -eq {batch} ]; then {inspect} "$@"; set --; fi
done
exit 1
)"#,
        batch = docker::INSPECT_BATCH
    ))
}

fn parse_containers(text: &str) -> Vec<Container> {
    let mut entries = Vec::new();
    let mut inspected = Vec::new();
    for line in text.lines() {
        if let Some(row) = line.strip_prefix('P') {
            entries.extend(docker::parse_ps(row));
        } else {
            inspected.extend(docker::parse_inspect(line));
        }
    }
    docker::merge(&entries, &inspected)
}

impl Task {
    pub(super) async fn prefetch_pages(&self, probe: &Probe) {
        if probe.systemd.as_ref().is_some_and(|systemd| systemd.running) {
            self.refresh_services(probe, &mut HashMap::new()).await;
        }
        if self.with(|_, _| ()).is_some()
            && let Some(runtime) = probe.usable_container_runtime()
        {
            self.refresh_docker(runtime, false, &mut Default::default()).await;
        }
    }
    /// Runs a line on the connection.
    async fn exec(&self, line: String) -> Result<ExecOutput, String> {
        #[cfg(target_os = "macos")]
        if self.local {
            use crate::local::runner;
            return runner::output(&mut runner::shell(&line), COMMAND_TIMEOUT, MAX_OUTPUT).await.map_err(|err| err.to_string());
        }
        let connection = self.connection().ok_or_else(|| "没有可用的 SSH 连接".to_string())?;
        let result =
            connection.exec_with(&line, ExecOptions { timeout: COMMAND_TIMEOUT, max_output: MAX_OUTPUT }).await;
        drop(connection);
        result.map_err(|err| err.to_string())
    }

    /// Starts the tasks that follow the services and the containers. They end when the guards are dropped.
    pub(super) fn start_pollers(&self, probe: &Arc<Probe>) -> Vec<AbortOnDrop> {
        let Some(inner) = self.inner.upgrade() else { return Vec::new() };
        fn spawn<F: std::future::Future<Output = ()> + Send + 'static>(inner: &Inner, future: F) -> AbortOnDrop {
            AbortOnDrop(inner.runtime.spawn(future))
        }
        let mut tasks = Vec::new();
        if probe.systemd.as_ref().is_some_and(|systemd| systemd.running) {
            tasks.push(spawn(
                &inner,
                self.clone().watch_services(probe.clone(), inner.focus.subscribe(), inner.wake_services.clone()),
            ));
        }
        if let Some(runtime) = probe.usable_container_runtime().cloned() {
            tasks.push(spawn(
                &inner,
                self.clone().watch_docker(
                    runtime.clone(),
                    inner.focus.subscribe(),
                    inner.wake_docker.clone(),
                    inner.docker_full.clone(),
                ),
            ));
            tasks.push(spawn(&inner, self.clone().watch_events(runtime.clone(), inner.wake_docker.clone())));
            tasks.push(spawn(&inner, self.clone().watch_stats(runtime, inner.focus.subscribe())));
        }
        tasks
    }

    // ---- services ------------------------------------------------------------------------------

    async fn watch_services(self, probe: Arc<Probe>, mut focus: watch::Receiver<Focus>, wake: Arc<Notify>) {
        // The last error line of a failed unit, by unit and the time it failed (looked up once).
        let mut errors: HashMap<(String, Option<u64>), Option<String>> = HashMap::new();
        let mut last: Option<Instant> = None;
        loop {
            let every =
                if *focus.borrow() == Focus::Services { self.options.poll_visible } else { self.options.poll_hidden };
            if last.is_none_or(|at| at.elapsed() >= every) {
                if !self.refresh_services(&probe, &mut errors).await {
                    return;
                }
                last = Some(Instant::now());
                continue;
            }
            let wait = every.saturating_sub(last.map_or(Duration::ZERO, |at| at.elapsed()));
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = wake.notified() => last = None,
                changed = focus.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    // Somebody opened the page: what is on it should not be older than a moment.
                    if *focus.borrow() == Focus::Services && last.is_some_and(|at| at.elapsed() > Duration::from_secs(2)) {
                        last = None;
                    }
                }
            }
        }
    }

    /// Asks systemd and reports the list. `false` when the monitor is gone.
    async fn refresh_services(
        &self,
        probe: &Probe,
        errors: &mut HashMap<(String, Option<u64>), Option<String>>,
    ) -> bool {
        let commands = [services::list_units().line(), services::list_unit_files().line()];
        let Ok(output) = self.exec(section_script(&commands)).await else {
            return self.inner.strong_count() > 0;
        };
        let text = output.stdout_text();
        let mut parts = sections(&text);
        let rows = services::parse_list_units(parts.next().unwrap_or_default());
        let unit_files = services::parse_unit_files(parts.next().unwrap_or_default());
        if rows.is_empty() && unit_files.is_empty() {
            return self.inner.strong_count() > 0;
        }
        let mut commands: Vec<_> = services::names_to_show(&rows, &unit_files, SHOW_LIMIT)
            .chunks(services::SHOW_BATCH)
            .filter_map(|batch| services::show(batch).ok().map(|command| command.line()))
            .collect();
        let show_count = commands.len();
        let access = probe.journal_access();
        let mut asked = Vec::new();
        if access != JournalAccess::None {
            for row in rows
                .iter()
                .filter(|row| row.active == "failed" && !errors.keys().any(|(name, _)| name == &row.name))
                .take(ERRORS_PER_ROUND)
            {
                if let Ok(command) = services::journal_last_error(&row.name) {
                    commands.push(if access == JournalAccess::Sudo { command.sudo_n() } else { command }.line());
                    asked.push(&row.name);
                }
            }
        }
        let details = self.exec(section_script(&commands)).await.ok().map(|out| out.stdout_text()).unwrap_or_default();
        let mut parts = sections(&details);
        let props: Vec<_> = parts.by_ref().take(show_count).flat_map(services::parse_show).collect();
        let mut list = services::merge(&rows, &unit_files, &props);
        for (name, text) in asked.into_iter().zip(parts) {
            if let Some(service) = list.iter().find(|service| &service.name == name && service.is_failed()) {
                errors.insert((name.clone(), service.since_monotonic_us), services::pick_error_line(text));
            }
        }
        errors.retain(|(name, since), _| {
            list.iter()
                .any(|service| service.is_failed() && service.name == *name && service.since_monotonic_us == *since)
        });
        for service in list.iter_mut().filter(|service| service.is_failed()) {
            service.last_error = errors.get(&(service.name.clone(), service.since_monotonic_us)).cloned().flatten();
        }
        let list = Arc::new(ServiceList { services: list });
        self.with(|inner, state| {
            if state.services.as_deref() != Some(&*list) {
                state.services = Some(list.clone());
                inner.emit(state, MonitorEvent::Services(list));
            }
        })
        .is_some()
    }

    // ---- containers ----------------------------------------------------------------------------

    async fn watch_docker(
        self,
        runtime: ContainerRuntime,
        mut focus: watch::Receiver<Focus>,
        wake: Arc<Notify>,
        full: Arc<AtomicBool>,
    ) {
        let mut last: Option<Instant> = None;
        let mut usage_at: Option<Instant> = None;
        // Containers that were killed for their memory in the restart loop they are in (see `remember_oom`).
        let mut oom: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            let visible = *focus.borrow() == Focus::Docker;
            let every = if visible { self.options.poll_visible } else { self.options.poll_hidden };
            let manual = full.swap(false, Ordering::Relaxed);
            if manual || last.is_none_or(|at| at.elapsed() >= every) {
                let want_usage =
                    manual || (visible && usage_at.is_none_or(|at| at.elapsed() >= self.options.usage_every));
                if want_usage {
                    usage_at = Some(Instant::now());
                }
                if !self.refresh_docker(&runtime, want_usage, &mut oom).await {
                    return;
                }
                last = Some(Instant::now());
                continue;
            }
            let wait = every.saturating_sub(last.map_or(Duration::ZERO, |at| at.elapsed()));
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = wake.notified() => {
                    // A restart makes several events in a row: ask once after they are over.
                    tokio::time::sleep(self.options.debounce).await;
                    if !full.load(Ordering::Relaxed) {
                        last = None;
                    }
                }
                changed = focus.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if *focus.borrow() == Focus::Docker {
                        // The page was opened: the disk usage is wanted too.
                        usage_at = None;
                        last = None;
                    }
                }
            }
        }
    }

    /// The command as the container tool is run: with `sudo -n` when only that gets it through.
    fn docker_line(runtime: &ContainerRuntime, command: Command) -> String {
        if runtime.access == ContainerAccess::Sudo { command.sudo_n().line() } else { command.line() }
    }

    /// Asks docker and reports the containers. `false` when the monitor is gone.
    async fn refresh_docker(
        &self,
        runtime: &ContainerRuntime,
        want_usage: bool,
        oom: &mut std::collections::HashSet<String>,
    ) -> bool {
        let tool = self.tool(runtime);
        let base = self.refresh_containers(runtime, oom);
        let usage = async {
            if !want_usage {
                return None;
            }
            let (df, dangling) = tokio::join!(
                self.exec(Self::docker_line(runtime, docker::system_df(tool))),
                self.exec(Self::docker_line(runtime, docker::dangling_images(tool)))
            );
            Some((
                df.ok().filter(|out| out.status.success()).map(|out| docker::parse_system_df(&out.stdout_text())),
                dangling.ok().filter(|out| out.status.success()).map(|out| docker::parse_dangling(&out.stdout_text())),
            ))
        };
        let (alive, usage) = tokio::join!(base, usage);
        if let Some((usage, dangling)) = usage {
            self.with(|inner, state| {
                apply_docker(inner, state, runtime, |base, _| {
                    if let Some(usage) = usage {
                        base.usage = usage;
                    }
                    if let Some(dangling) = dangling {
                        base.dangling = dangling;
                    }
                })
            });
        }
        alive
    }

    async fn refresh_containers(
        &self,
        runtime: &ContainerRuntime,
        oom: &mut std::collections::HashSet<String>,
    ) -> bool {
        let result = self.list_containers(runtime).await;
        let (containers, error) = match result {
            Ok(mut containers) => { remember_oom(&mut containers, oom); (Some(containers), None) }
            Err(why) => (None, Some(why)),
        };
        self.with(|inner, state| {
            if error.is_some() && !state.run_alive {
                return;
            }
            apply_docker(inner, state, runtime, |base, _| {
                base.error = error;
                if let Some(containers) = containers {
                    base.containers = containers;
                }
            });
        })
        .is_some()
    }

    fn tool<'a>(&self, runtime: &'a ContainerRuntime) -> &'a str {
        if self.local { &runtime.path } else { runtime.kind.command() }
    }

    async fn list_containers(&self, runtime: &ContainerRuntime) -> Result<Vec<Container>, String> {
        let checked = |output: ExecOutput| {
            if output.truncated { Err("容器列表超过输出上限".into()) }
            else if !output.status.success() { Err(explain_failure(&output)) }
            else { Ok(output.stdout_text()) }
        };
        if !self.local { return Ok(parse_containers(&checked(self.exec(docker_script(runtime)).await?)?)); }
        let tool = self.tool(runtime);
        let entries = docker::parse_ps(&checked(self.exec(docker::ps(tool).line()).await?)?);
        let ids = entries.iter().map(|entry| entry.id.clone()).collect::<Vec<_>>();
        let mut details = Vec::new();
        for batch in ids.chunks(docker::INSPECT_BATCH) {
            let command = docker::inspect(tool, batch).map_err(|err| err.to_string())?;
            if let Ok(text) = checked(self.exec(command.line()).await?) { details.extend(docker::parse_inspect(&text)); }
        }
        Ok(docker::merge(&entries,&details))
    }

    async fn stream(&self, line: String) -> Option<PollStream> {
        #[cfg(target_os = "macos")]
        if self.local { return crate::local::runner::Stream::open(&line).ok().map(|stream| PollStream::Local(Box::new(stream))); }
        self.connection()?.exec_stream(&crate::script::watched(&line)).await.ok().map(|stream| PollStream::Ssh(Box::new(stream)))
    }

    /// The `events` stream: every container event asks for a new look.
    async fn watch_events(self, runtime: ContainerRuntime, wake: Arc<Notify>) {
        loop {
            let line = Self::docker_line(&runtime, docker::events(self.tool(&runtime)));
            if let Some(mut stream) = self.stream(line).await {
                while let Some(line) = stream.next_line().await {
                    if docker::is_container_event(&line) {
                        wake.notify_one();
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            if self.inner.strong_count() == 0 {
                return;
            }
        }
    }

    /// The `stats` stream: only while the Docker page is in front.
    async fn watch_stats(self, runtime: ContainerRuntime, mut focus: watch::Receiver<Focus>) {
        loop {
            while *focus.borrow_and_update() != Focus::Docker {
                if focus.changed().await.is_err() {
                    return;
                }
            }
            let line = Self::docker_line(&runtime, docker::stats(self.tool(&runtime)));
            let stream = self.stream(line).await;
            if let Some(mut stream) = stream {
                // `stats` writes all containers and then, a second later, moves the cursor home (`ESC [ H`) and writes
                // them again: a batch ends where the next one begins (and every line ends with `ESC [ K`, which is
                // nothing to go by). A tool that does not move the cursor repeats a name: that ends the batch too.
                let mut batch: HashMap<String, StatsEntry> = HashMap::new();
                loop {
                    tokio::select! {
                        line = stream.next_line() => {
                            let Some(line) = line else { break };
                            if line.contains("\u{1b}[H") && !batch.is_empty() {
                                self.publish_stats(&runtime, std::mem::take(&mut batch));
                            }
                            if let Some(entry) = docker::parse_stats_line(&line) {
                                if batch.contains_key(&entry.name) {
                                    self.publish_stats(&runtime, std::mem::take(&mut batch));
                                }
                                batch.insert(entry.name.clone(), entry);
                            }
                        }
                        changed = focus.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            if *focus.borrow() != Focus::Docker {
                                break;
                            }
                        }
                    }
                }
                drop(stream);
                // Numbers from before are not shown as if they were current.
                self.publish_stats(&runtime, HashMap::new());
            }
            if *focus.borrow() == Focus::Docker {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }

    fn publish_stats(&self, runtime: &ContainerRuntime, stats: HashMap<String, StatsEntry>) {
        self.with(|inner, state| {
            if state.docker.is_some() {
                apply_docker(inner, state, runtime, |_, current| *current = stats);
            }
        });
    }
}

enum PollStream {
    Ssh(Box<shellmate_ssh::ExecStream>),
    #[cfg(target_os = "macos")]
    Local(Box<crate::local::runner::Stream>),
}

impl PollStream {
    async fn next_line(&mut self) -> Option<String> {
        match self {
            Self::Ssh(stream) => stream.next_line().await,
            #[cfg(target_os = "macos")]
            Self::Local(stream) => stream.next_line().await,
        }
    }
}

/// Docker clears `OOMKilled` when the restart policy starts the container again, so a container that is killed
/// for its memory again and again says so only half of the time. A container that was seen killed keeps the
/// flag for as long as it is in the loop (and is forgotten when it is not, or gone).
pub(super) fn remember_oom(containers: &mut [Container], seen: &mut std::collections::HashSet<String>) {
    for container in containers.iter_mut() {
        if container.oom_killed {
            seen.insert(container.id.clone());
        } else if seen.contains(&container.id) && container.is_restart_looping() {
            container.oom_killed = true;
        }
    }
    seen.retain(|id| containers.iter().any(|c| &c.id == id && (c.oom_killed || c.is_restart_looping())));
}

/// The reason a command failed: its stderr, or stdout, or the exit status.
fn explain_failure(output: &ExecOutput) -> String {
    let text = match output.stderr_text().trim() {
        "" => output.stdout_text(),
        stderr => stderr.to_string(),
    };
    let text = text.trim();
    if text.is_empty() {
        return match &output.status {
            ExecStatus::Code(code) => format!("退出码 {code}"),
            ExecStatus::Signal(signal) => format!("被信号 {signal} 结束"),
            ExecStatus::Unknown => "没有结果".into(),
        };
    }
    text.lines().take(2).collect::<Vec<_>>().join(" ").chars().take(300).collect()
}

// ---- following a command -----------------------------------------------------------------------

/// Runs `command` on its own exec channel and hands the lines over. See [`Monitor::follow`].
pub(super) fn follow(inner: &Arc<Inner>, command: String) -> Follow {
    let (tx, rx) = async_channel::bounded(1024);
    let slot = inner.slot.clone();
    let closed = lock(&inner.state).closed;
    #[cfg(target_os = "macos")]
    let local = inner.local;
    let task = inner.runtime.spawn(async move {
        #[cfg(target_os = "macos")]
        if local && !closed {
            let why = match crate::local::runner::Stream::open(&command) {
                Ok(mut stream) => {
                    while let Some(line) = stream.next_line().await {
                        if tx.send(FollowEvent::Line(line)).await.is_err() { return; }
                    }
                    stream.finish().await
                }
                Err(err) => err.to_string(),
            };
            let _ = tx.send(FollowEvent::Ended(why)).await;
            return;
        }
        let connection = lock(&slot).as_ref().and_then(WeakConnection::upgrade).filter(|_| !closed);
        let Some(connection) = connection else {
            let _ = tx.send(FollowEvent::Ended("没有可用的 SSH 连接".into())).await;
            return;
        };
        // Ends on the server when the follow is dropped, though the command may write nothing for hours.
        let stream = connection.exec_stream(&crate::script::watched(&command)).await;
        drop(connection);
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                let _ = tx.send(FollowEvent::Ended(err.to_string())).await;
                return;
            }
        };
        while let Some(line) = stream.next_line().await {
            if tx.send(FollowEvent::Line(line)).await.is_err() {
                return;
            }
        }
        let tail = stream.stderr_tail();
        let tail = tail.trim();
        let why = match (stream.status(), tail) {
            (_, tail) if !tail.is_empty() => tail.chars().take(300).collect(),
            (Some(ExecStatus::Code(0)) | None, _) => String::new(),
            (Some(ExecStatus::Code(code)), _) => format!("退出码 {code}"),
            (Some(ExecStatus::Signal(signal)), _) => format!("被信号 {signal} 结束"),
            (Some(ExecStatus::Unknown), _) => String::new(),
        };
        let _ = tx.send(FollowEvent::Ended(why)).await;
    });
    Follow { events: rx, _task: AbortOnDrop(task) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::ContainerState;

    #[test]
    fn marked_sections_keep_empty_answers_and_discard_a_truncated_tail() {
        let value = "two lines\na quote ' and $HOME";
        let command = Command::new(["printf", "%s", value]).unwrap();
        let script = section_script(&[command.line(), "false".into(), "printf last".into()]);
        let output = std::process::Command::new("sh").args(["-c", &script]).output().unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(sections(&text).collect::<Vec<_>>(), [value, "", "last"]);
        assert_eq!(sections(text.trim_end_matches('\0')).collect::<Vec<_>>(), [value, ""]);
    }

    #[test]
    fn docker_script_lists_once_batches_valid_ids_and_propagates_ps_failure() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join("docker");
        std::fs::write(
            &tool,
            r##"#!/bin/sh
case $1 in
ps)
  [ -z "$FAIL_PS" ] || exit 9
  index=0
  while [ "$index" -lt 101 ]; do
    printf '%x:{"ID":"%x","Names":"test"}\n' "$index" "$index"
    index=$((index+1))
  done
  printf '%s:{}\n' '-bad;exit 42'
  ;;
inspect)
  shift 3
  echo "$#" >> batches
  for id do printf '{"id":"%s","restarts":7}\n' "$id"; done
  ;;
esac
"##,
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = ContainerRuntime {
            kind: crate::probe::RuntimeKind::Docker,
            access: ContainerAccess::User,
            path: "docker".into(),
            version: None,
        };
        let mut shell = std::process::Command::new("/bin/sh");
        shell
            .current_dir(temp.path())
            .env("PATH", format!("{}:/bin", temp.path().display()))
            .args(["-c", &docker_script(&runtime)]);
        let out = shell.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let rows = parse_containers(&String::from_utf8(out.stdout).unwrap());
        assert_eq!(rows.len(), 101);
        assert!(rows.iter().all(|row| row.restart_count == 7));
        assert_eq!(std::fs::read_to_string(temp.path().join("batches")).unwrap(), "100\n1\n");
        assert!(!shell.env("FAIL_PS", "1").output().unwrap().status.success());
    }

    fn looping(id: &str, oom: bool) -> Container {
        Container {
            id: id.into(),
            name: id.into(),
            image: "x".into(),
            state: ContainerState::Restarting,
            status: "Restarting (137) 1 second ago".into(),
            ports: Vec::new(),
            project: None,
            config_files: None,
            service: None,
            restart_count: 4,
            oom_killed: oom,
            restarting: true,
            memory_limit: None,
            networks: Vec::new(),
            up_secs: None,
            cpu_percent: None,
            mem_used: None,
        }
    }

    #[test]
    fn a_container_killed_for_its_memory_keeps_the_flag_through_the_restarts() {
        let mut seen = std::collections::HashSet::new();
        let mut first = vec![looping("a", true), looping("b", false)];
        remember_oom(&mut first, &mut seen);
        assert!(first[0].oom_killed && !first[1].oom_killed);
        // The next look catches it between a start and the kill: docker says false, the memory says true.
        let mut second = vec![looping("a", false), looping("b", false)];
        remember_oom(&mut second, &mut seen);
        assert!(second[0].oom_killed, "still in the loop it was killed in");
        assert!(!second[1].oom_killed, "another container is not blamed");
        // Out of the loop: forgotten; a later loop for another reason is not an OOM.
        let mut calm = looping("a", false);
        calm.state = ContainerState::Running;
        calm.restarting = false;
        calm.restart_count = 0;
        let mut third = vec![calm];
        remember_oom(&mut third, &mut seen);
        assert!(!third[0].oom_killed && seen.is_empty());
        let mut again = vec![looping("a", false)];
        remember_oom(&mut again, &mut seen);
        assert!(!again[0].oom_killed);
        // A removed container is forgotten.
        let mut gone = vec![looping("z", true)];
        remember_oom(&mut gone, &mut seen);
        remember_oom(&mut [], &mut seen);
        assert!(seen.is_empty());
    }
}
