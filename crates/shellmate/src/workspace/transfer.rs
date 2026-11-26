//! Tab ownership changes are synchronous on GPUI's foreground thread.
use super::*;
use gpui::{AnyWindowHandle, Bounds, DisplayId, Pixels, point};
use crate::productivity::multi::{Broadcast, Split};

#[must_use]
pub(super) struct DetachedSession {
    session: Session,
    source: gpui::AnyWindowHandle,
    resume_typing: bool,
    question: Option<connect::UiMsg>,
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
    pub(super) fn detach_session(&mut self, id: usize, destination: AnyWindowHandle, window: &mut Window, cx: &mut Context<Self>) -> Option<DetachedSession> {
        let ix = self.sessions.iter().position(|s| s.id == id)?;
        if destination == self.handle {
            // Reordering keeps focus, subscriptions, split and broadcast in this window.
            return Some(DetachedSession {
                session: self.sessions.remove(ix), source: self.handle, resume_typing: false, question: None,
            });
        }
        let question = self.take_connection_prompt(id, window, cx);
        self.detach_clash_dialog(id, window, cx);
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
        self.tab_menu = None;
        self.picker = None;
        self.picker_subscription = None;
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
            question,
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
            question,
        } = detached;
        if source == self.handle {
            self.sessions.insert(index, session);
            return;
        }
        let id = session.id;
        // Moving only changes the UI owner. The terminal backend and AI lease
        // continue; commands and completion callbacks route through the Hub.
        session._subscription = Some(self.watch_terminal(id, &session.terminal, window, cx));
        // The session takes the font size of the window it lands in.
        let style = self.terminal_style(cx);
        session.view.update(cx, |view, cx| {
            view.rebind_window(window, cx);
            view.set_display_paused(self.hidden, cx);
            view.set_style(style, cx);
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
        if let Some(question) = question {
            cx.defer(move |cx| {
                if let Some(entry) = Route::Session(id).target(cx) {
                    entry.update(cx, |workspace, window, cx| workspace.open_connection_prompt(id, question, window, cx));
                }
            });
        }
        log::info!(
            "session {id} moved from window {} to {}",
            source.window_id().as_u64(),
            self.handle.window_id().as_u64()
        );
    }

    fn refresh_after_detach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_window_title(window, cx);
        self.update_pane_radii(cx);
        self.sync_ai_context(cx);
        let selected = self.active.and_then(|ix| self.sessions.get(ix)).map(|s| {
            s.host()
                .map_or_else(|| "local".into(), |h| h.id.to_string())
        });
        self.sidebar.update(cx, |s, cx| s.select(selected, cx));
        if !window.has_active_dialog(cx) {
            if self.active.is_some() {
                self.focus_active(window, cx);
            } else {
                window.focus(&self.focus_handle, cx);
            }
        }
        self.sync_sidebar(cx);
        self.sync_local_files_activity(cx);
        self.sync_monitor_activity(cx);
        cx.notify();
    }
}

pub(super) fn window_title(tab: Option<&str>) -> String {
    tab.map_or_else(|| "Shellmate".to_owned(), |name| format!("{name} — Shellmate"))
}

type Placement = (Bounds<Pixels>, Option<DisplayId>);

fn cascade(current: Bounds<Pixels>, screen: Bounds<Pixels>) -> Bounds<Pixels> {
    let shifted = Bounds::new(current.origin + point(px(28.), px(28.)), current.size);
    if shifted.left() < screen.left() || shifted.top() < screen.top()
        || shifted.right() > screen.right() || shifted.bottom() > screen.bottom()
    {
        Bounds::new(screen.origin, current.size)
    } else {
        shifted
    }
}

fn new_window_placement(window: &Window, cx: &App) -> Placement {
    let current = window.bounds();
    window.display(cx).or_else(|| cx.primary_display()).map_or(
        (Bounds::new(current.origin + point(px(28.), px(28.)), current.size), None),
        |display| (cascade(current, display.visible_bounds()), Some(display.id())),
    )
}

fn open_secondary((bounds, display_id): Placement, cx: &mut App) -> Option<hub::Entry> {
    match crate::open_workspace_at(false, None, bounds, display_id, false, cx) {
        Ok((handle, _)) => Hub::entry(handle, cx),
        Err(error) => {
            log::error!("could not open workspace window: {error:#}");
            None
        }
    }
}

impl Workspace {
    pub(super) fn new_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placement = new_window_placement(window, cx);
        cx.defer(move |cx| {
            if let Some(entry) = open_secondary(placement, cx) {
                entry.update(cx, Self::show_window);
            }
        });
    }

    pub(super) fn move_session_to_new_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.main_id() {
            move_to_new(self.handle, id, window, cx);
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Destination {
    New(Placement),
    NewForDrag(Placement, gpui::Point<Pixels>, gpui::Point<Pixels>),
    Existing(AnyWindowHandle, usize),
}

pub(super) fn move_to_new(source: AnyWindowHandle, id: usize, window: &Window, cx: &mut App) {
    let destination = Destination::New(new_window_placement(window, cx));
    cx.defer(move |cx| move_sessions(source, destination, vec![id], cx));
}

pub(super) fn move_sessions(handle: AnyWindowHandle, target: Destination, ids: Vec<usize>, cx: &mut App) {
    let Some(source) = Hub::entry(handle, cx) else { return };
    let Some(owner) = source.workspace.upgrade() else { return };
    let workspace = owner.read(cx);
    if ids.is_empty() || !ids.iter().all(|id| workspace.sessions.iter().any(|tab| tab.id == *id)) {
        return;
    }
    if let Some((_, editor)) = workspace.editor.as_ref().filter(|(id, _)| ids.contains(id)) {
        let editor = editor.clone();
        source.handle.update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.close_then(window, cx, move |cx| {
                move_sessions(handle, target, ids, cx);
            }));
        }).ok();
        return;
    }
    let (destination, index) = match target {
        Destination::New(placement) | Destination::NewForDrag(placement, _, _) => (open_secondary(placement, cx), 0),
        Destination::Existing(handle, index) => (Hub::entry(handle, cx), index),
    };
    if let Some(destination) = destination {
        if !move_tabs(&source, &destination, &ids, index, cx) {
            if matches!(target, Destination::New(_) | Destination::NewForDrag(..)) {
                destination.update(cx, |_, window, _| window.remove_window());
            }
        } else if let Destination::NewForDrag(_, pointer, grab) = target {
            super::tab_drag::place_new_tab(&destination, ids[0], pointer, grab, cx);
        }
    }
}

/// Open the destination's update scope before relinquishing source ownership.
fn move_tabs(
    source: &hub::Entry,
    destination: &hub::Entry,
    ids: &[usize],
    index: usize,
    cx: &mut App,
) -> bool {
    if source.handle == destination.handle || ids.is_empty() {
        return false;
    }
    let moved = destination.update(cx, |target, window, cx| {
        let Some(tabs) = source.update(cx, |workspace, source_window, cx| {
            ids.iter()
                .filter_map(|id| workspace.detach_session(*id, destination.handle, source_window, cx))
                .collect::<Vec<_>>()
        }) else {
            return false;
        };
        if tabs.is_empty() {
            return false;
        }
        target.show_window(window, cx);
        let index = index.min(target.sessions.len());
        let dialog_focus = window.has_active_dialog(cx).then(|| window.focused(cx)).flatten();
        for (offset, tab) in tabs.into_iter().enumerate() {
            target.attach_session(tab, index + offset, window, cx);
        }
        if let Some(focus) = dialog_focus {
            window.focus(&focus, cx);
        }
        true
    }).unwrap_or(false);
    if moved {
        source.update(cx, |workspace, window, cx| {
            if !workspace.close_empty_window_if_needed(window, cx) {
                workspace.refresh_after_detach(window, cx);
            }
        });
    }
    moved
}

#[cfg(debug_assertions)]
fn move_all(source: &hub::Entry, destination: &hub::Entry, cx: &mut App) {
    if let Some(workspace) = source.workspace.upgrade() {
        let ids: Vec<_> = workspace.read(cx).sessions.iter().map(|session| session.id).collect();
        if ids.is_empty() {
            source.update(cx, |workspace, window, cx| workspace.close_empty_window_if_needed(window, cx));
        } else {
            move_sessions(source.handle, Destination::Existing(destination.handle, usize::MAX), ids, cx);
        }
    }
}

pub(super) fn move_to_recent_window(source: AnyWindowHandle, tabs: Vec<usize>, cx: &mut App) {
    if let Some(destination) = Hub::most_recent_other(source, cx) {
        move_sessions(source, Destination::Existing(destination.handle, usize::MAX), tabs, cx);
    }
}

#[cfg(debug_assertions)]
pub(super) fn merge_back(cx: &mut App) {
    let windows = Hub::windows(cx);
    let first = windows.iter().min_by_key(|entry| entry.number).cloned();
    let newest = windows.iter().max_by_key(|entry| entry.number).cloned();
    if let (Some(first), Some(newest)) = (first, newest)
        && first.handle != newest.handle
    {
        move_all(&newest, &first, cx);
        first.update(cx, Workspace::show_window);
    }
}

/// The windows a prototype scene sets up around the main one.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DemoWindows {
    /// The last stream tab elsewhere; the window then closes.
    CloseDialog,
    /// The last two tabs in a window each; the tab menu opens with the list of windows.
    TabMenu,
    /// All but the first tab in one window: this one keeps a single session and the menu opens on it.
    TabMenuSingle,
}

#[cfg(debug_assertions)]
impl Workspace {
    pub(super) fn demo_multi_window(&mut self, demo: DemoWindows, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.handle;
        let current = window.bounds();
        let display = window.display(cx).map(|display| display.id());
        let close = demo == DemoWindows::CloseDialog;
        // Move fixture tabs to background windows using the production path.
        let groups: Vec<Vec<usize>> = if demo == DemoWindows::TabMenuSingle {
            vec![self.sessions.iter().skip(1).map(|session| session.id).collect()]
        } else {
            let count = if close { 1 } else { 2 };
            self.sessions.iter()
                .filter(|session| !close || matches!(session.kind, SessionKind::Stream(_)))
                .rev().take(count).map(|session| vec![session.id]).collect()
        };
        if close {
            for session in self.sessions.iter_mut().take(2) {
                if let SessionKind::Ssh(ssh) = &mut session.kind {
                    ssh.status = ssh::SshStatus::Connecting;
                }
            }
            if let Some(SessionKind::Ssh(ssh)) = self.sessions.last_mut().map(|session| &mut session.kind) {
                ssh.status = ssh::SshStatus::Failed;
            }
            self.sync_sidebar(cx);
        }
        cx.defer(move |cx| {
            let Some(source) = Hub::entry(source, cx) else { return };
            for (offset, ids) in groups.into_iter().enumerate() {
                let inset = px(50. + 28. * offset as f32);
                let bounds = Bounds::new(current.origin + point(inset, inset), gpui::size(px(980.), px(620.)));
                if let Some(destination) = open_secondary((bounds, display), cx) {
                    move_tabs(&source, &destination, &ids, 0, cx);
                }
            }
            source.update(cx, |workspace, window, cx| {
                workspace.show_window(window, cx);
                if close {
                    workspace.close_window(window, cx);
                } else {
                    workspace.demo_tab_menu(demo == DemoWindows::TabMenu, window, cx);
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_titles_follow_the_active_tab() {
        assert_eq!(window_title(None), "Shellmate");
        assert_eq!(window_title(Some("本机")), "本机 — Shellmate");
    }

    #[test]
    fn window_placement_offsets_or_wraps_with_the_same_size() {
        let size = gpui::size(px(1000.), px(700.));
        let screen = Bounds::new(point(px(0.), px(25.)), gpui::size(px(1500.), px(950.)));
        let current = Bounds::new(point(px(60.), px(60.)), size);
        assert_eq!(cascade(current, screen), Bounds::new(point(px(88.), px(88.)), size));
        let at_edge = Bounds::new(point(px(500.), px(60.)), size);
        assert_eq!(cascade(at_edge, screen), Bounds::new(screen.origin, size));
    }

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
