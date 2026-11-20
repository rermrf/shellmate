//! Tab ownership changes are synchronous on GPUI's foreground thread.
use super::*;
use crate::productivity::multi::{Broadcast, Split};

#[must_use]
pub(super) struct DetachedSession {
    session: Session,
    source: gpui::AnyWindowHandle,
    resume_typing: bool,
}

fn remove_tab(
    active: Option<usize>,
    ix: usize,
    remaining: usize,
    main: Option<usize>,
    id: usize,
    split: &mut Split,
    broadcast: &mut Broadcast,
) -> Option<usize> {
    split.close_session(main, id);
    broadcast.checked.remove(&id);
    let active = active?;
    remaining.checked_sub(1).map(|last| {
        if ix <= active {
            active.saturating_sub(1).min(last)
        } else {
            active
        }
    })
}

impl Workspace {
    pub(super) fn detach_session(&mut self, id: usize) -> Option<DetachedSession> {
        let ix = self.sessions.iter().position(|s| s.id == id)?;
        let main = self.main_id();
        let mut session = self.sessions.remove(ix);
        self.active = remove_tab(
            self.active,
            ix,
            self.sessions.len(),
            main,
            id,
            &mut self.split,
            &mut self.broadcast,
        );
        self.session_menu = None;
        self.picker = None;
        self.picker_subscription = None;
        session
            .ai_alive
            .store(false, std::sync::atomic::Ordering::Release);
        session._subscription = None;
        if let Some(tab) = &mut session.files {
            tab._subscription = None;
        }
        if let Some(tab) = &mut session.forward {
            tab._subscription = None;
        }
        if let Some(tab) = &mut session.monitor {
            tab._subscription = None;
        }
        let resume_typing = session.typing.take().is_some();
        Some(DetachedSession {
            session,
            source: self.handle,
            resume_typing,
        })
    }

    pub(super) fn attach_session(
        &mut self,
        detached: DetachedSession,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let DetachedSession {
            mut session,
            source,
            resume_typing,
        } = detached;
        let id = session.id;
        session.ai_alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        session
            .terminal
            .update(cx, |terminal, _| terminal.rebind_agent());
        session._subscription = Some(self.watch_terminal(id, &session.terminal, window, cx));
        session.view.update(cx, |view, cx| {
            view.rebind_window(window, cx);
            view.set_display_paused(self.hidden, cx);
        });
        if let Some(tab) = &mut session.files {
            tab.panel
                .update(cx, |panel, cx| panel.rebind_window(window, cx));
            tab._subscription = Some(Self::bind_files(id, &tab.panel, window, cx));
        }
        if let Some(tab) = &mut session.forward {
            tab.panel
                .update(cx, |panel, cx| panel.rebind_window(window, cx));
            tab._subscription = Some(Self::bind_forward(id, &tab.panel, window, cx));
        }
        if let Some(tab) = &mut session.monitor {
            tab.panel
                .update(cx, |panel, cx| panel.rebind_window(window, cx));
            tab._subscription = Some(Self::bind_monitor(id, &tab.panel, window, cx));
        }
        let index = index.min(self.sessions.len());
        if let Some(active) = &mut self.active
            && index <= *active
        {
            *active += 1;
        }
        self.sessions.insert(index, session);
        self.activate(index, window, cx);
        if resume_typing {
            self.type_pending(id, window, cx);
        }
        if let Some(entry) = Hub::windows(cx)
            .iter()
            .find(|e| e.handle == source)
            .cloned()
        {
            cx.defer(move |cx| {
                entry.update(cx, Self::refresh_after_detach);
            });
        }
        log::info!(
            "session {id} moved from window {} to {}",
            source.window_id().as_u64(),
            self.handle.window_id().as_u64()
        );
    }

    fn refresh_after_detach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_pane_radii(cx);
        self.sync_ai_context(cx);
        let selected = self.active.and_then(|ix| self.sessions.get(ix)).map(|s| {
            s.host()
                .map_or_else(|| "local".into(), |h| h.id.to_string())
        });
        self.sidebar.update(cx, |s, cx| s.select(selected, cx));
        if self.active.is_some() {
            self.focus_active(window, cx);
        } else {
            window.focus(&self.focus_handle, cx);
        }
        self.sync_sidebar(cx);
        self.sync_local_files_activity(cx);
        self.sync_monitor_activity(cx);
        cx.notify();
    }
}

/// Acquire the destination window before relinquishing source ownership.
#[cfg_attr(not(debug_assertions), allow(dead_code))] // Entry point for the next phase's tab UI.
fn move_tab(
    source: &hub::Entry,
    destination: &hub::Entry,
    id: usize,
    index: usize,
    cx: &mut App,
) -> bool {
    if source.handle == destination.handle {
        return false;
    }
    destination
        .update(cx, |w, window, cx| {
            let Some(source) = source.workspace.upgrade() else {
                return false;
            };
            let Some(tab) = source.update(cx, |s, _| s.detach_session(id)) else {
                return false;
            };
            w.attach_session(tab, index, window, cx);
            true
        })
        .unwrap_or(false)
}

#[cfg(debug_assertions)]
pub(super) fn move_to_new_window(cx: &mut App) {
    let Some(source) = Hub::windows(cx).first().cloned() else {
        return;
    };
    let Some(id) = source
        .workspace
        .upgrade()
        .and_then(|w| w.read(cx).main_id())
    else {
        return;
    };
    match crate::open_workspace(false, None, cx) {
        Ok((handle, workspace)) => {
            let destination = hub::Entry {
                handle,
                workspace: workspace.downgrade(),
            };
            if !move_tab(&source, &destination, id, 0, cx) {
                destination.update(cx, |_, w, _| w.remove_window());
            }
        }
        Err(e) => log::error!("could not open workspace window: {e:#}"),
    }
}

#[cfg(debug_assertions)]
pub(super) fn merge_back(cx: &mut App) {
    // Creation order is independent of key-window changes.
    let mut windows = Hub::windows(cx).to_vec();
    windows.sort_by_key(|e| e.workspace.entity_id());
    let (Some(first), Some(newest)) = (windows.first(), windows.last()) else {
        return;
    };
    if first.handle == newest.handle {
        return;
    }
    let Some(source) = newest.workspace.upgrade() else {
        return;
    };
    let ids: Vec<_> = source.read(cx).sessions.iter().map(|s| s.id).collect();
    for id in ids {
        move_tab(newest, first, id, usize::MAX, cx);
    }
    if source.read(cx).sessions.is_empty() {
        newest.update(cx, |_, w, _| w.remove_window());
        first.update(cx, Workspace::show_window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_a_pane_ends_the_split_and_forgets_its_broadcast_target() {
        let mut split = Split {
            other: Some(12),
            ..Default::default()
        };
        let mut broadcast = Broadcast {
            checked: [11, 12].into(),
            enabled: true,
        };
        assert_eq!(
            remove_tab(Some(1), 0, 1, Some(11), 12, &mut split, &mut broadcast),
            Some(0)
        );
        assert_eq!(split.other, None);
        assert_eq!(broadcast.checked, [11].into());
        assert_eq!(
            remove_tab(Some(0), 0, 0, Some(11), 11, &mut split, &mut broadcast),
            None
        );
    }
}
