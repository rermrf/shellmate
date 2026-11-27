use super::panel::conversation_runtime::Runtime;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
#[derive(Clone, Default)]
pub struct Manager(Arc<Mutex<BTreeMap<String, Arc<Runtime>>>>);
impl Manager {
    pub fn insert(&self, runtime: Arc<Runtime>) -> Arc<Runtime> {
        self.0
            .lock()
            .unwrap()
            .entry(runtime.id.clone())
            .or_insert(runtime)
            .clone()
    }
    pub fn get(&self, id: &str) -> Option<Arc<Runtime>> {
        self.0.lock().unwrap().get(id).cloned()
    }
    pub fn list(&self) -> Vec<Arc<Runtime>> {
        self.0.lock().unwrap().values().cloned().collect()
    }
    pub fn remove(&self, id: &str) {
        if let Some(r) = self.0.lock().unwrap().remove(id) {
            r.tasks.discard();
            r.retire();
        }
    }
    pub fn attention(&self) -> (usize, usize) {
        let mut pending = 0;
        let mut unread = 0;
        for r in self.list() {
            let d = r.data.lock().unwrap();
            pending += usize::from(
                d.approvals
                    .values()
                    .any(|a| a.reply.as_ref().is_some_and(|reply| !reply.is_closed())),
            );
            unread += usize::from(d.revision > r.seen.load(std::sync::atomic::Ordering::Acquire));
        }
        (pending, unread)
    }
    pub fn affected(&self, sessions: Option<&[usize]>) -> Vec<(String, String)> {
        self.list()
            .into_iter()
            .filter(|r| r.active())
            .filter(|r| {
                sessions.is_none_or(|ids| {
                    r.router.scope.lock().unwrap().targets.keys().any(|id| {
                        r.router
                            .catalog
                            .list()
                            .iter()
                            .any(|e| &e.target.session_id == id && ids.contains(&e.binding.session))
                    })
                })
            })
            .map(|r| {
                let title = r
                    .snapshot()
                    .model
                    .conversation
                    .turns()
                    .first()
                    .map(|t| t.user.title(36))
                    .unwrap_or_else(|| "新对话".into());
                (r.id.clone(), title)
            })
            .collect()
    }
    pub fn stop_all(&self) {
        for runtime in self.list() {
            runtime.retire();
        }
    }
    pub fn clear(&self) {
        for runtime in self.list() {
            self.remove(&runtime.id);
        }
    }
}
