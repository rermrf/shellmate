//! App-wide terminal directory. A reference identifies one connection generation.
use super::{exec::Binding, host::Ui};
use shellmate_ai::{ContextSnapshot, session_tools::Destination};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::Ordering},
};
#[derive(Clone)]
pub struct Endpoint {
    pub target: Destination,
    pub binding: Binding,
    pub window: String,
    pub connected: bool,
    pub lease: Arc<std::sync::atomic::AtomicBool>,
    pub slots: Arc<tokio::sync::Semaphore>,
}
#[derive(Clone)]
pub struct Catalog {
    inner: Arc<Mutex<State>>,
    web: Arc<std::sync::OnceLock<Arc<super::web::Service>>>,
    pub commands: Arc<tokio::sync::Semaphore>,
    pub models: Arc<tokio::sync::Semaphore>,
    pub bridge: async_channel::Sender<Ui>,
}
struct State {
    epoch: String,
    next: u64,
    entries: BTreeMap<usize, Endpoint>,
}
impl Catalog {
    pub fn web(
        &self,
        db: shellmate_store::SharedStore,
        credentials: Arc<dyn shellmate_store::SecretStore>,
    ) -> Arc<super::web::Service> {
        self.web
            .get_or_init(|| Arc::new(super::web::Service::new(db, credentials)))
            .clone()
    }
    pub fn update_window(
        &self,
        window: &str,
        entries: Vec<(Binding, String, String, Option<String>, String)>,
    ) {
        let mut all = self
            .list()
            .into_iter()
            .filter(|e| e.window != window)
            .map(|e| {
                (
                    e.binding,
                    e.target.label,
                    e.target.protocol,
                    e.target.host_id,
                    e.window,
                )
            })
            .collect::<Vec<_>>();
        all.extend(entries);
        self.update(all);
    }
    pub fn new(bridge: async_channel::Sender<Ui>) -> Self {
        Self {
            web: Default::default(),
            commands: Arc::new(tokio::sync::Semaphore::new(12)),
            models: Arc::new(tokio::sync::Semaphore::new(4)),
            inner: Arc::new(Mutex::new(State {
                epoch: shellmate_core::HostId::generate().to_string(),
                next: 0,
                entries: BTreeMap::new(),
            })),
            bridge,
        }
    }
    pub fn update(&self, entries: Vec<(Binding, String, String, Option<String>, String)>) {
        let mut state = self.inner.lock().unwrap();
        let mut next: BTreeMap<usize, Endpoint> = BTreeMap::new();
        for (binding, label, protocol, host_id, window) in entries {
            let connected = binding.alive.load(Ordering::Acquire)
                && (binding.capabilities.terminal || binding.capabilities.background);
            let reference = state
                .entries
                .get(&binding.session)
                .filter(|e| {
                    e.connected == connected
                        && Arc::ptr_eq(&e.binding.alive, &binding.alive)
                        && match (&e.binding.connection, &binding.connection) {
                            (Some(a), Some(b)) => a.same_transport(b),
                            (None, None) => true,
                            _ => false,
                        }
                })
                .map(|e| e.target.session_id.clone())
                .unwrap_or_else(|| {
                    state.next += 1;
                    format!("s-{}-{}", state.epoch, state.next)
                });
            let lease = state
                .entries
                .get(&binding.session)
                .filter(|e| e.target.session_id == reference)
                .map(|e| e.lease.clone())
                .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(connected)));
            let slots = if let Some(connection) = &binding.connection {
                next.values()
                    .chain(state.entries.values())
                    .find(|e| {
                        e.binding
                            .connection
                            .as_ref()
                            .is_some_and(|c| c.same_transport(connection))
                    })
                    .map(|e| e.slots.clone())
                    .unwrap_or_else(|| Arc::new(tokio::sync::Semaphore::new(4)))
            } else {
                self.commands.clone()
            };
            next.insert(
                binding.session,
                Endpoint {
                    target: Destination {
                        session_id: reference,
                        label,
                        protocol,
                        host_id,
                        cwd: binding.capabilities.cwd.clone(),
                    },
                    binding,
                    window,
                    connected,
                    lease,
                    slots,
                },
            );
        }
        for (id, old) in &state.entries {
            if next
                .get(id)
                .is_none_or(|e| e.target.session_id != old.target.session_id)
            {
                old.lease.store(false, Ordering::Release);
            }
        }
        state.entries = next;
    }
    pub fn list(&self) -> Vec<Endpoint> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .values()
            .cloned()
            .collect()
    }
    pub fn session(&self, id: usize) -> Option<Endpoint> {
        self.inner.lock().unwrap().entries.get(&id).cloned()
    }
    pub fn resolve(&self, reference: &str) -> Result<Endpoint, String> {
        self.list()
            .into_iter()
            .find(|e| e.target.session_id == reference)
            .filter(|e| e.connected && e.binding.alive.load(Ordering::Acquire))
            .ok_or_else(|| "目标终端已关闭、断线或连接已变化；请重新选择终端".into())
    }
    pub async fn read(&self, reference: &str, lines: usize) -> Result<ContextSnapshot, String> {
        let endpoint = self.resolve(reference)?;
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.bridge
            .send(Ui::ReadSession {
                session: endpoint.binding.session,
                lease: endpoint.binding.alive.clone(),
                lines,
                reply,
            })
            .await
            .map_err(|_| "终端桥接已关闭")?;
        let result = rx.await.map_err(|_| "终端读取已取消")?;
        self.resolve(reference)?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(
        id: usize,
        alive: Arc<std::sync::atomic::AtomicBool>,
        name: &str,
        window: &str,
    ) -> (Binding, String, String, Option<String>, String) {
        (
            Binding {
                session: id,
                connection: None,
                local: true,
                alive,
                capabilities: shellmate_ai::Capabilities {
                    background: true,
                    cwd: Some("/tmp".into()),
                    ..Default::default()
                },
            },
            name.into(),
            "本机终端".into(),
            None,
            window.into(),
        )
    }
    #[test]
    fn references_survive_window_move_but_not_reconnect_or_restart() {
        let (tx, _) = async_channel::unbounded();
        let c = Catalog::new(tx);
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        c.update(vec![entry(1, alive.clone(), "name", "window1")]);
        let lease = c.session(1).unwrap().lease;
        let old = c.session(1).unwrap().target.session_id;
        c.update(vec![entry(1, alive, "renamed", "window2")]);
        assert_eq!(c.session(1).unwrap().target.session_id, old);
        c.update(vec![entry(
            1,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            "renamed",
            "window2",
        )]);
        assert!(!lease.load(Ordering::Acquire));
        assert!(c.resolve(&old).is_err());
        assert_ne!(c.session(1).unwrap().target.session_id, old);
        c.update(vec![]);
        assert!(c.session(1).is_none());
    }
    #[test]
    fn incremental_window_move_preserves_running_lease_and_reference() {
        let (tx, _) = async_channel::unbounded();
        let c = Catalog::new(tx);
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        c.update_window("one", vec![entry(1, alive.clone(), "local", "one")]);
        let original = c.session(1).unwrap();
        c.update_window("two", vec![entry(1, alive, "local", "two")]);
        c.update_window("one", vec![]);
        let moved = c.resolve(&original.target.session_id).unwrap();
        assert_eq!(moved.window, "two");
        assert!(Arc::ptr_eq(&moved.lease, &original.lease));
        assert!(original.lease.load(Ordering::Acquire));
        c.update_window("two", vec![]);
        assert!(!original.lease.load(Ordering::Acquire));
    }
}
