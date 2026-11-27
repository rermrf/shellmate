//! Conversation-owned interactive processes. Child workers share this registry.
use super::{
    host::Ui,
    session_catalog::Endpoint,
    tasks::{Kind, Status, TaskInfo},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
#[derive(Default)]
pub struct Pool {
    pub retired: AtomicBool,
    bridge: OnceLock<async_channel::Sender<Ui>>,
    records: Mutex<BTreeMap<String, Arc<Record>>>,
}
pub struct Record {
    pub endpoint: Endpoint,
    pub info: Mutex<TaskInfo>,
    pub stopped: AtomicBool,
}
impl Pool {
    pub fn register(
        &self,
        endpoint: Endpoint,
        command: String,
        call_id: String,
        source: Option<String>,
        bridge: async_channel::Sender<Ui>,
    ) -> Result<Arc<Record>, String> {
        let mut records = self.records.lock().unwrap();
        if self.retired.load(Ordering::Acquire) {
            return Err("会话已关闭".into());
        }
        if records.len() >= 8 {
            return Err("本会话已有 8 个独立终端，请关闭不再使用的终端".into());
        }
        let _ = self.bridge.set(bridge);
        let id = endpoint.target.session_id.clone();
        let record = Arc::new(Record {
            endpoint: endpoint.clone(),
            stopped: AtomicBool::new(false),
            info: Mutex::new(TaskInfo {
                service: None,
                destination: Some(endpoint.target),
                owner: source,
                kind: Kind::Command,
                call_id,
                id: id.clone(),
                command,
                target: shellmate_ai::Target::Background,
                started: Instant::now(),
                status: Status::Running,
                output_id: id.clone(),
                background: true,
            }),
        });
        records.insert(id, record.clone());
        Ok(record)
    }
    pub fn get(&self, id: &str) -> Option<Arc<Record>> {
        self.records.lock().unwrap().get(id).cloned()
    }
    pub fn list(&self) -> Vec<TaskInfo> {
        self.records
            .lock()
            .unwrap()
            .values()
            .map(|r| r.info.lock().unwrap().clone())
            .collect()
    }
    pub fn remove(&self, id: &str) {
        self.records.lock().unwrap().remove(id);
    }
    pub fn stop(&self, id: &str) -> bool {
        let Some(r) = self.get(id) else {
            return false;
        };
        r.stopped.store(true, Ordering::Release);
        if let Some(tx) = self.bridge.get() {
            let _ = tx.try_send(Ui::StopPty {
                session: r.endpoint.binding.session,
                close: false,
            });
        }
        true
    }
    pub fn stop_all(&self) {
        for t in self.list() {
            self.stop(&t.id);
        }
    }
    pub fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.close_all();
    }
    fn close_all(&self) {
        if let Some(tx) = self.bridge.get() {
            for r in self.records.lock().unwrap().values() {
                r.stopped.store(true, Ordering::Release);
                let _ = tx.try_send(Ui::StopPty {
                    session: r.endpoint.binding.session,
                    close: true,
                });
            }
        }
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.close_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{exec::Binding, session_catalog::Catalog, tasks::Tasks};
    pub(crate) fn endpoint(catalog: &Catalog) -> Endpoint {
        catalog.update(vec![(
            Binding {
                session: 1,
                local: true,
                connection: None,
                alive: Arc::new(AtomicBool::new(true)),
                capabilities: shellmate_ai::Capabilities {
                    terminal: true,
                    background: true,
                    cwd: Some("/tmp".into()),
                    ..Default::default()
                },
            },
            "local".into(),
            "本机终端".into(),
            None,
            "window".into(),
        )]);
        catalog.list()[0].clone()
    }
    #[test]
    fn independent_resources_survive_source_close_and_child_finish_until_conversation_retirement() {
        let dir = tempfile::tempdir().unwrap();
        let root = Tasks::new(dir.path().into(), "conversation".into());
        let child = root.child_tasks();
        let (tx, rx) = async_channel::unbounded();
        let catalog = Catalog::new(tx.clone());
        let parent = endpoint(&catalog);
        let mut e = parent.clone();
        e.binding.alive = Arc::new(AtomicBool::new(true));
        let e = catalog.insert_independent(e).unwrap();
        let pool = child.ptys();
        let record = pool
            .register(
                e.clone(),
                "vim example".into(),
                "create".into(),
                Some("agent-1".into()),
                tx,
            )
            .unwrap();
        child.finish_agent_commands();
        drop(child);
        catalog.update(vec![]);
        assert!(catalog.resolve(&e.target.session_id).is_ok());
        assert!(catalog.resolve(&parent.target.session_id).is_err());
        assert_eq!(root.list().len(), 1);
        assert!(!record.stopped.load(Ordering::Acquire));
        assert!(rx.try_recv().is_err());
        assert!(root.stop(&e.target.session_id));
        assert!(record.stopped.load(Ordering::Acquire));
        assert!(matches!(
            rx.try_recv().unwrap(),
            Ui::StopPty { close: false, .. }
        ));
        record
            .endpoint
            .binding
            .alive
            .store(false, Ordering::Release);
        assert!(catalog.resolve(&e.target.session_id).is_err());
        assert!(catalog.lookup(&e.target.session_id).is_ok());
        root.retire();
        assert!(pool.retired.load(Ordering::Acquire));
        assert!(matches!(
            rx.try_recv().unwrap(),
            Ui::StopPty { close: true, .. }
        ));
        assert!(
            pool.register(
                e,
                "late".into(),
                "late".into(),
                None,
                catalog.bridge.clone()
            )
            .is_err()
        );
    }
}
