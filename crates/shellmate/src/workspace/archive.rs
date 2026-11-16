//! Session lifecycle, recording catalog, and immediate preferences application.
use super::*;
use shellmate_store::{Preference as P, Recording};
use shellmate_term::{archive, cast::Header};
use std::time::{Duration, Instant};
pub(super) struct RunningRecording {
    pub row: Recording,
    pub start: Instant,
    #[cfg(debug_assertions)]
    pub demo_elapsed: Option<f64>,
}
impl RunningRecording {
    pub fn display_seconds(&self) -> f64 {
        #[cfg(debug_assertions)]
        if let Some(seconds) = self.demo_elapsed {
            return seconds;
        }
        self.start.elapsed().as_secs_f64()
    }
}
impl Workspace {
    pub(super) fn archive_timer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.archive_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this
                    .update_in(cx, |w, window, cx| w.tick_archives(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        self._subscriptions.push(cx.on_release(|w, cx| {
            for session in &w.sessions {
                if let Some(r) = w.running_recordings.remove(&session.id) {
                    let result = session.terminal.update(cx, |t, _| t.stop_recording());
                    let (duration, size) = match result {
                        Ok(Some(r)) => (r.duration, r.size),
                        _ => (
                            r.row.duration,
                            std::fs::metadata(&r.row.path).map_or(0, |m| m.len()),
                        ),
                    };
                    let _ = w.store.borrow_mut().finish_recording(r.row.id, duration, size);
                }
                session.terminal.update(cx, |t, _| {
                    t.finalize_log();
                    t.flush_archives();
                });
            }
        }));
        self._subscriptions.push(cx.on_app_quit(|w, cx| {
            let ids = w.sessions.iter().map(|s| s.id).collect::<Vec<_>>();
            for id in ids {
                w.finalize_archives(id, cx);
            }
            async {}
        }));
    }
    pub(super) fn start_session_archives(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_terminal_preferences(id, cx);
        if self.preferences.get(P::SessionLog) {
            self.set_session_logging(id, true, cx);
        }
        if self.preferences.get(P::AutoRecord)
            && let Err(e) = self.start_recording(id, cx)
        {
            push_notification_later(window, cx, Notification::error(e));
        }
    }
    fn apply_terminal_preferences(&self, id: usize, cx: &mut Context<Self>) {
        if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
            s.view.update(cx, |v, cx| {
                v.copy_on_select = self.preferences.get(P::CopySelection);
                v.right_click_paste = self.preferences.get(P::RightPaste);
                v.history_shortcut = self.preferences.get(P::HistoryShortcut);
                cx.notify();
            });
            s.terminal.update(cx, |t, _| {
                t.set_bracketed_paste(self.preferences.get(P::BracketedPaste))
            });
        }
    }
    fn set_session_logging(&self, id: usize, on: bool, cx: &mut Context<Self>) {
        if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
            let label = s.host().map_or("本机", |h| h.name.as_str());
            s.terminal
                .update(cx, |t, _| t.set_logging(on.then(|| self.log_root.clone()), label));
        }
    }
    pub(super) fn start_recording(&mut self, id: usize, cx: &mut Context<Self>) -> Result<(), String> {
        if self.running_recordings.contains_key(&id) {
            return Ok(());
        }
        let session = self.sessions.iter().find(|s| s.id == id).ok_or("会话已关闭")?;
        let (host, label) = session.host().map_or_else(
            || ("local".into(), "本机".into()),
            |h| (h.id.to_string(), h.name.clone()),
        );
        let now = archive::now();
        let size = session.terminal.read(cx).size();
        let (path, file) = archive::recording_file(&self.data_root, &label, now)
            .map_err(|e| format!("无法创建录制：{e}"))?;
        let title = format!("{label} · {}", archive::local_time(now, "%m-%d %H:%M"));
        let mut header = Header::new(size.cols, size.rows);
        header.timestamp = Some(now);
        header.title = Some(title.clone());
        header.env.insert("TERM".into(), "xterm-256color".into());
        if session.host().is_none() {
            let shell = session
                .terminal
                .read(cx)
                .options()
                .map_or_else(|| PtyOptions::default().shell_path(), |o| o.shell_path());
            header.env.insert("SHELL".into(), shell);
        }
        session
            .terminal
            .update(cx, |t, _| t.start_recording(path.clone(), file, &header))
            .map_err(|e| format!("无法开始录制：{e}"))?;
        let mut row = Recording {
            id: 0,
            title,
            host,
            label,
            path: path.to_string_lossy().into_owned(),
            started_at: now,
            duration: 0.,
            size: 0,
            cols: size.cols,
            rows: size.rows,
        };
        match self.store.borrow_mut().add_recording(&row) {
            Ok(id) => row.id = id,
            Err(e) => {
                let _ = session.terminal.update(cx, |t, _| t.stop_recording());
                let _ = std::fs::remove_file(path);
                return Err(format!("无法保存录制索引：{e}"));
            }
        }
        self.running_recordings.insert(
            id,
            RunningRecording {
                row,
                start: Instant::now(),
                #[cfg(debug_assertions)]
                demo_elapsed: None,
            },
        );
        cx.notify();
        Ok(())
    }
    pub(super) fn stop_recording(
        &mut self,
        id: usize,
        cx: &mut Context<Self>,
    ) -> Result<Option<i64>, String> {
        let Some(running) = self.running_recordings.remove(&id) else {
            return Ok(None);
        };
        let mut duration = running.row.duration;
        let mut size = std::fs::metadata(&running.row.path).map_or(0, |m| m.len());
        let mut error = None;
        if let Some(session) = self.sessions.iter().find(|s| s.id == id) {
            match session.terminal.update(cx, |t, _| t.stop_recording()) {
                Ok(Some(result)) => {
                    duration = result.duration;
                    size = result.size;
                }
                Ok(None) => {}
                Err(e) => error = Some(format!("录制保存失败：{e}")),
            }
        }
        self.store
            .borrow_mut()
            .finish_recording(running.row.id, duration, size)
            .map_err(|e| e.to_string())?;
        cx.notify();
        if let Some(e) = error {
            Err(e)
        } else {
            Ok(Some(running.row.id))
        }
    }
    pub(super) fn toggle_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.focused_session().map(|s| s.id) else {
            return;
        };
        if self.running_recordings.contains_key(&id) {
            match self.stop_recording(id, cx) {
                Ok(_) => {
                    window.push_notification(Notification::success("已保存 .cast · 可在「记录」中回放"), cx)
                }
                Err(e) => window.push_notification(Notification::error(e), cx),
            }
        } else if let Err(e) = self.start_recording(id, cx) {
            window.push_notification(Notification::error(e), cx);
        }
    }
    pub(super) fn finalize_archives(&mut self, id: usize, cx: &mut Context<Self>) -> Option<String> {
        let mut error = self.stop_recording(id, cx).err();
        if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
            let log_error = s.terminal.update(cx, |t, _| {
                t.finalize_log();
                t.flush_archives()
            });
            error = error.or(log_error);
        }
        error
    }
    fn tick_archives(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.poll_streams(window,cx);
        self.poll_mosh_connections(window,cx);
        let mut stopped = vec![];
        for s in &self.sessions {
            let error = s.terminal.update(cx, |t, _| t.flush_archives());
            if let Some(e) = error {
                window.push_notification(Notification::error(e), cx);
            }
            if let Some(r) = self.running_recordings.get_mut(&s.id) {
                if s.terminal.read(cx).recording_seconds().is_none() {
                    stopped.push(s.id);
                } else {
                    let size = std::fs::metadata(&r.row.path).map_or(0, |m| m.len());
                    r.row.duration = s.terminal.read(cx).recording_duration().unwrap_or(r.row.duration);
                    let _ = self
                        .store
                        .borrow_mut()
                        .finish_recording(r.row.id, r.row.duration, size);
                }
            }
        }
        for id in stopped {
            let _ = self.stop_recording(id, cx);
        }
        if !self.running_recordings.is_empty() {
            cx.notify();
        }
    }
    pub(crate) fn set_preference(
        &mut self,
        key: P,
        value: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.store
            .borrow_mut()
            .set_preference(key, value)
            .map_err(|e| e.to_string())?;
        self.preferences.set(key, value);
        let ids = self.sessions.iter().map(|s| s.id).collect::<Vec<_>>();
        for id in ids {
            self.apply_terminal_preferences(id, cx);
            if key == P::SessionLog {
                self.set_session_logging(id, value, cx);
            }
        }
        if key == P::FollowDirectory {
            for session in &self.sessions {
                if let Some(files) = &session.files {
                    files.panel.update(cx, |p, cx| p.set_follow(value, cx));
                }
            }
        }
        if key == P::History {
            self.refresh_suggestions(cx);
        }
        self.tick_archives(window, cx);
        cx.notify();
        Ok(())
    }
    pub(crate) fn clear_history(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        self.store
            .borrow_mut()
            .clear_history()
            .map_err(|e| e.to_string())?;
        self.picker = None;
        self.refresh_suggestions(cx);
        cx.notify();
        Ok(())
    }
    pub(crate) fn credentials(&self)->std::sync::Arc<shellmate_store::CredentialStore>{self.secrets.clone()}
    pub(crate) fn refresh_bundle(&mut self,w:&mut Window,cx:&mut Context<Self>){self.reload_book(w,cx);self.refresh_snippets(cx);}
    pub(super) fn open_settings(&mut self, section: usize, window: &mut Window, cx: &mut Context<Self>) {
        crate::settings::SettingsWindow::open(
            cx.entity().downgrade(),
            self.preferences.clone(),
            self.secrets.clone(),
            section,
            window,
            cx,
        );
    }
    pub(super) fn open_records(
        &mut self,
        selected: Option<i64>,
        logs: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.running_recordings.values().map(|r| r.row.id).collect();
        let view = crate::records::panel::RecordsWindow::open(
            self.store.clone(),
            self.log_root.clone(),
            selected,
            logs,
            active,
            window,
            cx,
        );
        self.records_window = Some(view.downgrade());
    }
}
