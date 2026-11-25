//! A small durable outbox bridges failed SQLite writes and conversation reloads.
//! Events become visible to the model only after the database accepts them.
use super::*;
use shellmate_ai::interaction::Completion;
use std::io::{self, Read, Write};

#[derive(Default)]
pub(super) struct Outbox {
    pending: BTreeMap<String, Completion>,
    loaded: bool,
    error: Option<&'static str>,
}
fn path(root: &std::path::Path, conversation: &str) -> io::Result<PathBuf> {
    Ok(ai_output::directory(root, conversation)?.join("task-events.json"))
}
impl Outbox {
    fn load(&mut self, path: &std::path::Path) -> io::Result<()> {
        if self.loaded {
            return Ok(());
        }
        match std::fs::File::open(path) {
            Ok(file) => {
                let mut bytes = vec![];
                file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 16 * 1024 * 1024 {
                    return Err(io::Error::other("任务通知记录过大"));
                }
                let saved: BTreeMap<String, Completion> =
                    serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                for (id, event) in saved {
                    self.pending.entry(id).or_insert(event);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.loaded = true;
        Ok(())
    }
    fn save(&self, path: &std::path::Path) -> io::Result<()> {
        if self.pending.is_empty() {
            match std::fs::remove_file(path) {
                Ok(()) => std::fs::File::open(path.parent().unwrap())?.sync_all(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        } else {
            let parent = path.parent().unwrap();
            std::fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            serde_json::to_writer(file.as_file_mut(), &self.pending).map_err(io::Error::other)?;
            file.flush()?;
            file.as_file().sync_all()?;
            file.persist(path).map_err(|e| e.error)?;
            std::fs::File::open(parent)?.sync_all()
        }
    }
}
impl Inner {
    pub(super) fn publish(&self, id: String, task_id: String, text: String) {
        self.flush_completions(Some(Completion {
            id,
            task_id,
            text,
            claimed: false,
        }));
    }
    fn flush_completions(&self, incoming: Option<Completion>) {
        let discarded = self.discarded.lock().unwrap();
        if *discarded {
            return;
        }
        let Some(store) = self.store.get() else {
            if let Some(e) = incoming {
                self.control.publish(e.id, e.task_id, e.text);
            }
            return;
        };
        let mut outbox = self.events.lock().unwrap();
        if let Some(e) = incoming {
            outbox.pending.entry(e.id.clone()).or_insert(e);
        }
        if outbox.loaded && outbox.pending.is_empty() && outbox.error.is_none() {
            return;
        }
        let result = (|| -> anyhow::Result<()> {
            let path = path(&self.root, &self.conversation)?;
            let mut db = store.borrow_mut();
            // Recheck privacy and deletion before creating or replaying any file.
            let saving = db
                .setting(shellmate_store::ai::SAVE_CONVERSATIONS)?
                .as_deref()
                != Some("0");
            let exists = db.ai_conversation_exists(&self.conversation)?;
            if !saving || !exists {
                let pending = outbox.pending.clone();
                outbox.pending.clear();
                outbox.loaded = true;
                if let Err(e) = outbox.save(&path) {
                    outbox.pending = pending;
                    return Err(e.into());
                }
                if !saving {
                    for (_, e) in pending {
                        self.control.publish(e.id, e.task_id, e.text);
                    }
                }
                return Ok(());
            }
            outbox.load(&path)?;
            if outbox.pending.is_empty() {
                outbox.save(&path)?;
                return Ok(());
            }
            // Write before attempting SQLite, so a process restart can retry.
            outbox.save(&path)?;
            let runtime = db.ai_runtime(&self.conversation)?;
            let acknowledged = runtime
                .as_ref()
                .and_then(|r| r["queue"]["delivered"].as_array());
            let mut delivered = vec![];
            for e in outbox.pending.values() {
                if acknowledged.is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(&e.id))) {
                    delivered.push((e.clone(), false));
                    continue;
                }
                db.save_ai_task_event(&self.conversation, &e.id, &e.task_id, &e.text)?;
                delivered.push((e.clone(), true));
            }
            for (e, publish) in delivered {
                outbox.pending.remove(&e.id);
                if publish {
                    self.control.publish(e.id, e.task_id, e.text);
                }
            }
            outbox.save(&path)?;
            Ok(())
        })();
        if let Err(e) = result {
            if outbox.error.is_none() {
                log::warn!("save task completion; retaining for retry: {e}");
            }
            outbox.error = Some("后台结果尚未保存，正在重试；任务结果仍可查看");
        } else {
            outbox.error = None;
        }
    }
}
impl Tasks {
    pub fn schedule_completion_retry(&self, runtime: &shellmate_ssh::SshRuntime) {
        use std::sync::atomic::Ordering;
        if self.0.store.get().is_none() {
            return;
        }
        let Ok(outbox) = self.0.events.try_lock() else {
            return;
        };
        if outbox.loaded && outbox.pending.is_empty() && outbox.error.is_none() {
            return;
        }
        drop(outbox);
        if self.0.event_retrying.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(&self.0);
        runtime.spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.flush_completions(None);
                    inner.event_retrying.store(false, Ordering::Release);
                }
            })
            .await;
        });
    }
    pub fn retry_completions(&self) {
        self.0.flush_completions(None);
    }
    pub fn completion_save_error(&self) -> Option<&'static str> {
        self.0
            .events
            .try_lock()
            .ok()
            .and_then(|outbox| outbox.error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shellmate_store::{HostStore, SharedStore, ai::SavedConversation};

    fn fixture() -> (
        tempfile::TempDir,
        SharedStore,
        rusqlite::Connection,
        SavedConversation,
    ) {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("hosts.db");
        let mut db = HostStore::open(&db_path).unwrap();
        let mut conversation = shellmate_ai::Conversation::default();
        conversation.push_user(shellmate_ai::UserTurn::new("task", Default::default()));
        let saved = SavedConversation {
            id: "conversation".into(),
            host_id: None,
            label: "local".into(),
            title: "task".into(),
            provider: "openai".into(),
            model: "fixture".into(),
            created_at: 1,
            updated_at: 1,
            conversation,
        };
        db.save_ai_conversation(&saved).unwrap();
        let connection = rusqlite::Connection::open(db_path).unwrap();
        (root, SharedStore::new(db), connection, saved)
    }
    fn block(connection: &rusqlite::Connection) {
        connection.execute_batch("CREATE TRIGGER fail_event BEFORE INSERT ON ai_task_events BEGIN SELECT RAISE(FAIL, 'simulated write failure'); END;").unwrap();
    }
    fn publish(tasks: &Tasks) {
        tasks
            .0
            .publish("output:one".into(), "task-1".into(), "finished".into());
    }

    #[test]
    fn failed_database_write_survives_registry_restart_and_retries_once() {
        let (root, db, connection, _) = fixture();
        let tasks = Tasks::new(root.path().into(), "conversation".into());
        tasks.set_store(db.clone());
        block(&connection);
        publish(&tasks);
        publish(&tasks);
        assert!(!tasks.control().pending_completions());
        assert!(tasks.completion_save_error().is_some());
        assert!(path(root.path(), "conversation").unwrap().is_file());
        assert!(
            db.borrow()
                .ai_task_events("conversation")
                .unwrap()
                .is_empty()
        );
        drop(tasks);
        let restored = Tasks::new(root.path().into(), "conversation".into());
        restored.set_store(db.clone());
        assert!(!restored.control().pending_completions());
        connection
            .execute_batch("DROP TRIGGER fail_event;")
            .unwrap();
        restored.retry_completions();
        restored.retry_completions();
        assert!(restored.completion_save_error().is_none());
        assert_eq!(restored.control().snapshot().completions.len(), 1);
        assert_eq!(db.borrow().ai_task_events("conversation").unwrap().len(), 1);
        assert!(!path(root.path(), "conversation").unwrap().exists());
    }

    #[test]
    fn stale_outbox_after_ack_is_not_replayed_or_reinserted() {
        let (root, db, connection, saved) = fixture();
        let tasks = Tasks::new(root.path().into(), "conversation".into());
        tasks.set_store(db.clone());
        block(&connection);
        publish(&tasks);
        let file = path(root.path(), "conversation").unwrap();
        let stale = std::fs::read(&file).unwrap();
        connection
            .execute_batch("DROP TRIGGER fail_event;")
            .unwrap();
        tasks.retry_completions();
        let snapshot = tasks
            .control()
            .delivery_snapshot(&[], &["output:one".into()]);
        db.borrow_mut()
            .save_ai_conversation_with_runtime(
                &saved,
                Some(&serde_json::json!({"queue": snapshot})),
            )
            .unwrap();
        drop(tasks);
        std::fs::write(&file, stale).unwrap();
        let restored = Tasks::new(root.path().into(), "conversation".into());
        restored.set_store(db.clone());
        assert!(!restored.control().pending_completions());
        assert!(
            db.borrow()
                .ai_task_events("conversation")
                .unwrap()
                .is_empty()
        );
        assert!(!file.exists());
    }

    #[test]
    fn deleted_or_private_conversation_does_not_replay_durable_outbox() {
        for deleted in [true, false] {
            let (root, db, connection, _) = fixture();
            let tasks = Tasks::new(root.path().into(), "conversation".into());
            tasks.set_store(db.clone());
            block(&connection);
            publish(&tasks);
            drop(tasks);
            if deleted {
                db.borrow_mut()
                    .delete_ai_conversation("conversation")
                    .unwrap();
            } else {
                db.borrow_mut()
                    .set_setting(shellmate_store::ai::SAVE_CONVERSATIONS, "0")
                    .unwrap();
            }
            let restored = Tasks::new(root.path().into(), "conversation".into());
            restored.set_store(db.clone());
            restored.retry_completions();
            assert!(!restored.control().pending_completions());
            assert!(
                db.borrow()
                    .ai_task_events("conversation")
                    .unwrap()
                    .is_empty()
            );
            assert!(!path(root.path(), "conversation").unwrap().exists());
            if deleted {
                assert!(!db.borrow().ai_conversation_exists("conversation").unwrap());
            } else {
                publish(&restored);
                assert!(restored.control().pending_completions());
                assert!(!path(root.path(), "conversation").unwrap().exists());
            }
        }
    }

    #[test]
    fn failed_journal_write_keeps_event_pending_and_cannot_wake_model() {
        let (root, db, _, _) = fixture();
        let tasks = Tasks::new(root.path().into(), "conversation".into());
        tasks.set_store(db.clone());
        let file = path(root.path(), "conversation").unwrap();
        std::fs::create_dir_all(&file).unwrap();
        publish(&tasks);
        assert!(!tasks.control().pending_completions());
        assert!(tasks.completion_save_error().is_some());
        std::fs::remove_dir(&file).unwrap();
        tasks.retry_completions();
        assert!(tasks.control().pending_completions());
        assert!(tasks.completion_save_error().is_none());
    }
}
