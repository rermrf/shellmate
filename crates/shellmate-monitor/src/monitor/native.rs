use super::*;
use crate::local::{Sampler, host_probe, ports, runner};
use crate::ports::{Listener, PortSources};
use tokio::time::{Instant, sleep_until};

impl Inner {
    pub(super) fn sync_native(self: &Arc<Self>, state: &mut State) {
        self.sync_ports(state);
        let inventory = self.inventory.clone();
        self.sync_services_with(state, move |snapshot, previous| {
            let inventory = inventory.clone();
            async move { inventory.lock().await.sample(snapshot, previous).await }
        });
    }

    fn sync_services_with<Fut>(self: &Arc<Self>, state: &mut State,
        mut sample: impl FnMut(Option<Arc<Snapshot>>, Option<Arc<ServiceList>>) -> Fut + Send + 'static)
    where Fut: Future<Output = ServiceList> + Send {
        let focus = *self.focus.borrow();
        if state.closed || !state.active || !matches!(focus, Focus::Services | Focus::Overview) {
            state.services_task = None;
            return;
        }
        if state.services_task.as_ref().is_some_and(|(current, _)| *current == focus) { return; }
        let period = if focus == Focus::Services { self.options.poll_visible } else { self.options.poll_hidden };
        let weak = Arc::downgrade(self);
        let wake = self.wake_services.clone();
        state.services_task = Some((focus, AbortOnDrop(self.runtime.spawn(async move {
            loop {
                let Some((snapshot, previous)) = weak.upgrade().map(|inner| {
                    let state = lock(&inner.state);
                    (state.snapshot.clone(), state.services.clone())
                }) else { return };
                let list = Arc::new(sample(snapshot, previous).await);
                {
                    let Some(inner) = weak.upgrade() else { return };
                    let mut state = lock(&inner.state);
                    if state.services_task.as_ref().is_none_or(|(_, task)| task.0.id() != tokio::task::id()) { return; }
                    if state.services.as_ref() != Some(&list) {
                        state.services = Some(list.clone());
                        inner.emit(&state, MonitorEvent::Services(list));
                    }
                }
                tokio::select! { () = tokio::time::sleep(period) => {}, () = wake.notified() => {} }
            }
        }))));
    }

    pub(super) fn sync_ports(self: &Arc<Self>, state: &mut State) {
        self.sync_ports_with(state, Duration::from_secs(5), ports::sample);
    }

    fn sync_ports_with<Fut>(
        self: &Arc<Self>,
        state: &mut State,
        period: Duration,
        mut sample: impl FnMut() -> Fut + Send + 'static,
    ) where
        Fut: Future<Output = std::io::Result<(PortSources, Vec<Listener>)>> + Send,
    {
        if state.closed || !state.active || *self.focus.borrow() != Focus::Ports {
            state.ports_task = None;
        } else if state.ports_task.is_none() {
            let weak = Arc::downgrade(self);
            state.ports_task = Some(AbortOnDrop(self.runtime.spawn(async move {
                let mut ticks = tokio::time::interval(period);
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticks.tick().await;
                    let result = sample().await;
                    let Some(inner) = weak.upgrade() else { return };
                    let mut state = lock(&inner.state);
                    if state
                        .ports_task
                        .as_ref()
                        .is_none_or(|task| task.0.id() != tokio::task::id())
                    {
                        return;
                    }
                    let snapshot = state.snapshot.clone();
                    let list = Arc::make_mut(state.ports.as_mut().expect("local port list"));
                    match result {
                        Ok((sources, mut rows)) => {
                            for row in &mut rows {
                                row.owner_start = row.owner(snapshot.as_deref()).map(|proc| proc.start_time);
                            }
                            list.rows = Some(Arc::new(rows));
                            list.sources = sources;
                            list.error = None;
                        }
                        Err(err) => list.error = Some(err.to_string()),
                    }
                    inner.emit(&state, MonitorEvent::Ports);
                }
            })));
        }
    }
}

impl Monitor {
    /// Follow the selected job's configured file while the pane owns the returned guard.
    pub fn follow_service_file(&self, key: &str, path: &str) -> Option<Follow> {
        if !self.inner.local || !crate::services::launchd::safe_path(path) { return None; }
        let list = self.services()?;
        if list.get(key)?.launchd.as_ref()?.log.as_deref() != Some(path) { return None; }
        let path = path.to_owned();
        let (tx, events) = async_channel::bounded(300);
        let task = self.inner.runtime.spawn(async move {
            if let Err(err) = crate::local::services::follow_file(&path, &tx).await {
                let _ = tx.send(FollowEvent::Ended(err.to_string())).await;
            }
        });
        Some(Follow { events, _task: AbortOnDrop(task) })
    }

    /// Re-resolve the confirmed words against the latest domain-qualified inventory before dispatch.
    pub fn run_local_service(&self, key: &str, action: crate::commands::ServiceAction, expected: &crate::commands::Command) -> Option<ActionId> {
        if !self.inner.local { return None; }
        let list = self.services()?;
        let (system, words) = crate::local::services::validated(&list, key, action, expected)?;
        let timeout = self.inner.options.action_timeout;
        Some(self.spawn_action(words.line(), crate::local::services::dispatch(system, words, move |mut cmd| async move {
            runner::output(&mut cmd, timeout, 64 << 10).await
        })))
    }
}

impl Task {
    pub(super) async fn run_local(self) {
        let probe = host_probe();
        let mut sampler = Sampler::default();
        sampler.snapshot.host.os_name = probe.os_name.clone();
        sampler.snapshot.host.kernel.clone_from(&probe.kernel);
        let probe = Arc::new(probe);
        let Some(_docker) = self.with(|inner, state| {
            state.probe = Some(probe.clone());
            state.docker = None;
            inner.emit(state, MonitorEvent::Probe(probe.clone()));
            AbortOnDrop(inner.runtime.spawn(self.clone().local_docker(probe)))
        }) else {
            return;
        };
        let mut system_at = Instant::now();
        let mut processes_at = system_at;
        let mut warm = true;
        loop {
            sleep_until(system_at.min(processes_at)).await;
            let now = Instant::now();
            let system = now >= system_at;
            let processes = now >= processes_at;
            if system {
                sampler = match tokio::task::spawn_blocking(move || {
                    sampler.system();
                    sampler
                })
                .await
                {
                    Ok(sampler) => sampler,
                    Err(err) => return self.finish(Phase::Failed(err.to_string())),
                };
            }
            let error = if processes {
                sampler.processes().await.err()
            } else {
                None
            };
            let snapshot = Arc::new(sampler.snapshot.clone());
            if self
                .with(|inner, state| {
                    state.snapshot = Some(snapshot.clone());
                    inner.emit(state, MonitorEvent::Snapshot(snapshot));
                    inner.set_phase(state, Phase::Collecting);
                })
                .is_none()
            {
                return;
            }
            if let Some(err) = error {
                return self.finish(Phase::Failed(format!("无法读取 /bin/ps：{err}")));
            }
            if warm {
                system_at = Instant::now() + Duration::from_millis(500);
                processes_at = system_at;
                warm = false;
            } else {
                if system {
                    system_at = now + Duration::from_secs(2);
                }
                if processes {
                    processes_at = now + Duration::from_secs(3);
                }
            }
        }
    }

    async fn local_docker(self, mut probe: Arc<Probe>) {
        let Some(mut focus) = self.with(|inner, _| inner.focus.subscribe()) else {
            return;
        };
        let mut watchers = Vec::new();
        loop {
            let runtime = match runner::docker_path() {
                Some(path) => Some(runner::docker_probe(&path).await),
                None => None,
            };
            let containers = runtime.into_iter().collect::<Vec<_>>();
            if containers != probe.containers {
                watchers.clear();
                Arc::make_mut(&mut probe).containers = containers;
                if self
                    .with(|inner, state| {
                        state.docker = None;
                        state.probe = Some(probe.clone());
                        inner.emit(state, MonitorEvent::Probe(probe.clone()));
                    })
                    .is_none()
                {
                    return;
                }
                watchers = self.start_pollers(&probe);
            }
            loop {
                tokio::select! {
                    () = tokio::time::sleep(self.options.poll_hidden) => break,
                    changed = focus.changed() => {
                        if changed.is_err() { return; }
                        if *focus.borrow() == Focus::Docker { break; }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::PortSource;
    use tokio::sync::oneshot;

    #[tokio::test(flavor = "current_thread")]
    async fn ordering_out_cancels_grace_and_reopening_starts_a_fresh_run() {
        // No yields: the executor never polls a system sampler or launches a process.
        let (events, _) = async_channel::unbounded();
        let monitor = Monitor::local(Handle::current(), events);
        monitor.set_active(true);
        let old_run = monitor.inner.task(&lock(&monitor.inner.state));
        monitor.set_active(false);
        assert!(lock(&monitor.inner.state).grace.is_some());
        monitor.set_window_hidden(true);
        assert_eq!(monitor.phase(), Phase::Idle);
        assert!(old_run.with(|_, _| ()).is_none());
        monitor.set_active(true);
        assert!(!lock(&monitor.inner.state).run_alive);
        monitor.set_window_hidden(false);
        monitor.set_active(true);
        assert_eq!(monitor.phase(), Phase::Starting);
        assert!(lock(&monitor.inner.state).run_alive);
        monitor.shutdown();
    }

    #[tokio::test]
    async fn service_polling_wakes_for_actions_and_cancels_when_hidden() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (events, changes) = async_channel::unbounded();
            let monitor = Monitor::local(Handle::current(), events);
            let inner = &monitor.inner;
            let (requests, samples) = async_channel::unbounded();
            let focus = |active, focus| {
                let requests = requests.clone();
                let mut state = lock(&inner.state);
                state.active = active;
                inner.focus.send_replace(focus);
                inner.sync_services_with(&mut state, move |_, _| {
                    let (reply, result) = oneshot::channel();
                    requests.try_send(reply).unwrap();
                    async { result.await.unwrap() }
                });
            };
            focus(false, Focus::Services);
            focus(true, Focus::Docker);
            assert!(samples.is_empty());
            focus(true, Focus::Overview);
            let mut overview = samples.recv().await.unwrap();
            focus(true, Focus::Services);
            overview.closed().await;
            samples.recv().await.unwrap().send(ServiceList::default()).unwrap();
            assert!(matches!(changes.recv().await.unwrap(), MonitorEvent::Services(_)));
            monitor.refresh(Topic::Services);
            let mut refresh = samples.recv().await.unwrap();
            focus(true, Focus::Processes);
            refresh.closed().await;
            focus(true, Focus::Services);
            let mut reopened = samples.recv().await.unwrap();
            monitor.set_active(false);
            reopened.closed().await;
            assert!(changes.is_empty());
        }).await.unwrap();
    }

    #[tokio::test]
    async fn ports_cancel_on_hide_and_preserve_last_success_until_recovery() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (events, changes) = async_channel::unbounded();
            let monitor = Monitor::local(Handle::current(), events);
            let inner = &monitor.inner;
            let (requested, requests) = async_channel::unbounded();
            let start = |period| {
                let requested = requested.clone();
                let mut state = lock(&inner.state);
                state.active = true;
                inner.focus.send_replace(Focus::Ports);
                inner.sync_ports_with(&mut state, period, move || {
                    let (reply, result) = oneshot::channel();
                    requested.try_send(reply).unwrap();
                    async { result.await.unwrap() }
                });
            };
            monitor.set_focus(Focus::Ports);
            assert!(lock(&inner.state).ports_task.is_none());
            start(Duration::from_millis(5));
            let mixed = PortSources { tcp: PortSource::Lsof, udp: PortSource::Netstat };
            requests.recv().await.unwrap().send(Ok((mixed, Vec::new()))).unwrap();
            assert!(matches!(changes.recv().await.unwrap(), MonitorEvent::Ports));
            let first = monitor.ports().unwrap().rows.clone().unwrap();
            requests
                .recv()
                .await
                .unwrap()
                .send(Err(std::io::Error::other("denied")))
                .unwrap();
            assert!(matches!(changes.recv().await.unwrap(), MonitorEvent::Ports));
            let failed = monitor.ports().unwrap();
            assert_eq!(failed.sources, mixed);
            assert_eq!(failed.error.as_deref(), Some("denied"));
            assert!(Arc::ptr_eq(failed.rows.as_ref().unwrap(), &first));
            let mut in_flight = requests.recv().await.unwrap();
            monitor.set_focus(Focus::Processes);
            in_flight.closed().await;
            assert!(lock(&inner.state).ports_task.is_none());
            start(Duration::from_secs(60));
            requests.recv().await.unwrap().send(Ok((PortSources::default(), Vec::new()))).unwrap();
            assert!(matches!(changes.recv().await.unwrap(), MonitorEvent::Ports));
            assert!(monitor.ports().unwrap().error.is_none());
            assert_eq!(monitor.ports().unwrap().sources, PortSources::default());
            monitor.set_active(false);
            assert!(lock(&inner.state).ports_task.is_none());
            assert!(requests.is_empty());
        })
        .await
        .expect("immediate sample, cancelled in-flight reader, and immediate reopen");
    }
}
