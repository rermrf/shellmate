use super::*;
use crate::local::{Sampler, host_probe, runner};
use tokio::time::{Instant, sleep_until};

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
