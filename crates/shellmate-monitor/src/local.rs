//! Native macOS measurements. The Linux frame protocol stays on the SSH side; both produce Snapshot.
mod processes;
pub(crate) mod runner;
mod system;

use crate::{CpuCard, NetworkCard, Snapshot};
use std::time::Instant;
use system::Interfaces;
pub(crate) use system::host_probe;

fn push<T>(history: &mut Vec<T>, value: T) {
    if history.len() == crate::snapshot::HISTORY_POINTS {
        history.remove(0);
    }
    history.push(value);
}

fn cpu_delta(before: &[[u32; 4]], after: &[[u32; 4]], card: &mut CpuCard) {
    let mut sum = (0u64, 0u64);
    card.per_core = after
        .iter()
        .enumerate()
        .map(|(index, current)| {
            let old = before.get(index)?;
            let delta = std::array::from_fn::<_, 4, _>(|field| u64::from(current[field].wrapping_sub(old[field])));
            let total = delta.iter().sum::<u64>();
            let busy = total - delta[libc::CPU_STATE_IDLE as usize];
            sum.0 += busy;
            sum.1 += total;
            (total > 0).then(|| 100.0 * busy as f32 / total as f32)
        })
        .collect();
    card.percent = (sum.1 > 0).then(|| 100.0 * sum.0 as f32 / sum.1 as f32);
    if let Some(value) = card.percent {
        push(&mut card.history, value);
    }
}

fn net_delta(before: &Interfaces, after: &Interfaces, elapsed: f64) -> Option<(f64, f64)> {
    let mut common = false;
    let (mut rx, mut tx) = (0., 0.);
    for (index, &(received, sent)) in after {
        if let Some(&(prev_rx, prev_tx)) = before.get(index)
            && let (Some(down), Some(up)) = (received.checked_sub(prev_rx), sent.checked_sub(prev_tx))
        {
            common = true;
            rx += down as f64;
            tx += up as f64;
        }
    }
    (common && elapsed > 0.).then(|| (rx / elapsed, tx / elapsed))
}

#[derive(Default)]
pub(crate) struct Sampler {
    pub(crate) snapshot: Snapshot,
    host: system::HostPort,
    cores: Vec<[u32; 4]>,
    interfaces: Option<(Instant, Interfaces)>,
    processes: processes::Processes,
}

impl Sampler {
    pub(crate) fn system(&mut self) {
        let s = &mut self.snapshot;
        if let Some(cores) = system::cpu(&self.host) {
            s.host.cores = cores.len();
            cpu_delta(&self.cores, &cores, s.cpu.get_or_insert_with(Default::default));
            self.cores = cores;
        }
        if let Some(memory) = system::memory(&self.host) {
            s.memory = Some(memory);
        }
        if let Some(load) = system::load() {
            s.host.load = Some(load);
        }
        if let Some(uptime) = system::uptime() {
            s.host.uptime_secs = uptime;
        }
        if let Some(disks) = system::disks() {
            s.disks = disks;
        }
        if let Some(interfaces) = system::network() {
            let now = Instant::now();
            let rates = self
                .interfaces
                .as_ref()
                .and_then(|(before, counters)| net_delta(counters, &interfaces, (now - *before).as_secs_f64()));
            let card = s.network.get_or_insert_with(|| NetworkCard {
                interface: "非回环接口合计".into(),
                ..Default::default()
            });
            card.rx_per_sec = rates.map(|pair| pair.0);
            card.tx_per_sec = rates.map(|pair| pair.1);
            if let Some((rx, tx)) = rates {
                push(&mut card.rx_history, rx);
                push(&mut card.tx_history, tx);
            }
            self.interfaces = Some((now, interfaces));
        }
    }

    pub(crate) async fn processes(&mut self) -> std::io::Result<()> {
        let rows = self
            .processes
            .sample(self.snapshot.memory.map_or(0, |mem| mem.total))
            .await?;
        self.snapshot.zombies = rows.iter().filter(|row| row.state == 'Z').count();
        self.snapshot.processes = rows;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_ticks_per_core_and_weighted_total() {
        let mut card = CpuCard::default();
        cpu_delta(&[], &[[0; 4]; 2], &mut card);
        assert_eq!(card.per_core, [None, None]);
        cpu_delta(&[[0; 4]; 2], &[[10, 10, 20, 0], [0, 0, 60, 0]], &mut card);
        assert_eq!(card.per_core, [Some(50.), Some(0.)]);
        assert_eq!(card.history, [20.]);
    }

    #[test]
    fn network_deltas_ignore_new_removed_and_reset_interfaces() {
        let large = 1u64 << 40;
        let prev = Interfaces::from([(1, (large, 10)), (2, (900, 100)), (3, (100, 100))]);
        let next = Interfaces::from([(1, (large + 80, 50)), (3, (0, 0)), (4, (large, large))]);
        assert_eq!(net_delta(&prev, &next, 2.), Some((40., 20.)));
    }

    #[tokio::test]
    async fn live_two_samples_include_this_process() {
        let mut sampler = Sampler::default();
        for index in 0..2 {
            if index == 1 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            sampler.system();
            sampler.processes().await.expect("/bin/ps must be executable");
        }
        let s = sampler.snapshot;
        assert!(s.host.cores > 0);
        let mem = s.memory.unwrap();
        assert!(mem.total > 0 && mem.used <= mem.total);
        assert!(!s.process(std::process::id()).unwrap().name.is_empty());
    }
}
