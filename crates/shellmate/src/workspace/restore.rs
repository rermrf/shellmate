//! Saving the windows and tabs of this run, and opening them again at the next launch.
//!
//! What is decided lives in `crate::restore` (the record, the plan, the waiting of a restored remote
//! tab, the throttle) and is tested there. This is the part that looks at the windows, writes the
//! store and opens the tabs, through the same functions a person's clicks use: a local tab is
//! `open_local_in`, a connection is what `connect_host` and ⌘⇧T make, a Telnet or serial tab is
//! `open_stream`, a quick connection is `quick_connect`, the remote directory is typed in by the
//! `cd -- … && clear` line of ⌘⇧T.
use super::*;
use crate::restore::snapshot::{self, KEY};
use crate::restore::{
    Deferred, Event, Frame, Located, Open, Plan, Screen, Source, Step, TabState, Wait, WindowPlan,
    WindowState,
};
use gpui::{Bounds, DisplayId, Pixels, point, size};
use shellmate_core::{Host, Protocol};
use std::time::Instant;

/// A demo scene or fixture run neither saves nor restores.
pub(super) fn demo_run() -> bool {
    std::env::var_os("SHELLMATE_DEMO").is_some()
}

// ---- screens and windows -------------------------------------------------------------------

fn frame_of(bounds: Bounds<Pixels>) -> Frame {
    let round = |pixels: Pixels| f32::from(pixels).round() as i32;
    Frame {
        x: round(bounds.origin.x),
        y: round(bounds.origin.y),
        w: round(bounds.size.width),
        h: round(bounds.size.height),
    }
}

fn screens(cx: &App) -> Vec<Screen> {
    let primary = cx.primary_display().map(|display| display.id());
    cx.displays()
        .iter()
        .filter_map(|display| {
            Some(Screen {
                id: display.uuid().ok()?.to_string(),
                area: frame_of(display.visible_bounds()),
                primary: Some(display.id()) == primary,
            })
        })
        .collect()
}

fn display_named(id: &str, cx: &App) -> Option<DisplayId> {
    cx.displays()
        .iter()
        .find(|display| display.uuid().is_ok_and(|uuid| uuid.to_string() == id))
        .map(|display| display.id())
}

/// The bounds and display a window of the plan opens with.
pub(crate) fn launch_bounds(place: &Located, cx: &App) -> (Bounds<Pixels>, Option<DisplayId>) {
    let Frame { x, y, w, h } = place.frame;
    let bounds = Bounds::new(
        point(px(x as f32), px(y as f32)),
        size(px(w as f32), px(h as f32)),
    );
    (
        bounds,
        place.screen.as_deref().and_then(|id| display_named(id, cx)),
    )
}

fn open_restored_window(place: &Located, cx: &mut App) -> Option<hub::Entry> {
    let (bounds, display) = launch_bounds(place, cx);
    match crate::open_workspace_at(false, None, bounds, display, false, cx) {
        Ok((handle, _)) => Hub::entry(handle, cx),
        Err(error) => {
            log::error!("restore: could not open a window: {error:#}");
            None
        }
    }
}

// ---- reading the record at launch ----------------------------------------------------------

/// The record to restore from: the file `SHELLMATE_DEBUG_RESTORE_FROM` names (debug builds), or the store.
fn saved_record(services: &Services) -> Option<String> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("SHELLMATE_DEBUG_RESTORE_FROM") {
        return match std::fs::read_to_string(&path) {
            Ok(text) => {
                log::info!(
                    "restore: reading the snapshot from {}",
                    std::path::Path::new(&path).display()
                );
                Some(text)
            }
            Err(error) => {
                log::warn!(
                    "restore: cannot read {}: {error}",
                    std::path::Path::new(&path).display()
                );
                None
            }
        };
    }
    match services.store.borrow().setting(KEY) {
        Ok(text) => text,
        Err(error) => {
            log::warn!("restore: cannot read the saved session: {error}");
            None
        }
    }
}

/// What the launch restores, if anything: needs the setting on, a record that can be read and has
/// tabs in it, and hosts that can be looked up. Otherwise the app starts the usual way (and says why in
/// the log). When it returns a plan, saving is held back until [`apply`] has opened it.
pub(crate) fn prepare(cx: &mut App) -> Option<Plan> {
    let services = cx.global::<Hub>().services.clone();
    if !services.saver.borrow().enabled() {
        log::info!("restore: off (the setting is off, or this is a demo run)");
        return None;
    }
    let Some(text) = saved_record(&services) else {
        log::info!("restore: no saved session, starting with a local terminal");
        return None;
    };
    let snapshot = match snapshot::Snapshot::from_json(&text) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            log::warn!("restore: the saved session is {error}; starting with a local terminal");
            return None;
        }
    };
    if snapshot.tab_count() == 0 {
        log::info!("restore: the saved session has no tabs, starting with a local terminal");
        return None;
    }
    let book = match services.store.borrow().book() {
        Ok(book) => book,
        Err(error) => {
            log::warn!("restore: cannot read the hosts ({error}); starting with a local terminal");
            return None;
        }
    };
    let plan = crate::restore::plan::plan(&snapshot, &book, &screens(cx), &|dir| {
        Path::new(dir).is_dir()
    });
    for note in &plan.notes {
        log::info!("restore: {note}");
    }
    if plan.is_empty() {
        log::info!("restore: nothing is left of the saved session, starting with a local terminal");
        return None;
    }
    log::info!("restore: plan: {}", plan.summary());
    services.saver.borrow_mut().begin_restore();
    Some(plan)
}

/// Opens the windows of the plan. The first one is `first`, which was opened already (at the place of
/// the first window of the plan); the others are opened here, behind it, in the order of the record.
pub(crate) fn apply(plan: Plan, first: gpui::AnyWindowHandle, cx: &mut App) {
    let message = crate::restore::plan::missing_message(&plan.missing_hosts);
    for (index, window_plan) in plan.windows.iter().enumerate() {
        let entry = if index == 0 {
            Hub::entry(first, cx)
        } else {
            open_restored_window(&window_plan.place, cx)
        };
        let Some(entry) = entry else {
            log::warn!("restore: window {} could not be opened", index + 1);
            continue;
        };
        let message = message.clone().filter(|_| index == 0);
        entry.update(cx, |workspace, window, cx| {
            workspace.restore_window(index + 1, window_plan, window, cx);
            if let Some(message) = message {
                log::info!("restore: {message}");
                push_notification_later(
                    window,
                    cx,
                    Notification::warning(message).title("恢复会话"),
                );
            }
        });
    }
    // The first window of the record is the one used last: it is the front one, and the key one.
    Hub::activated(first, cx);
    if let Some(entry) = Hub::entry(first, cx) {
        entry.update(cx, |_, window, _| window.activate_window());
    }
    cx.global::<Hub>().services.saver.borrow_mut().end_restore();
    log::info!("restore: done");
}

// ---- opening the windows -------------------------------------------------------------------

impl Workspace {
    fn restore_window(
        &mut self,
        number: usize,
        plan: &WindowPlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Frame { x, y, w, h } = plan.place.frame;
        log::info!(
            "restore: window {number}: {w}x{h} at {x},{y} on screen {}, sidebar {}, {} tab(s), current {}",
            plan.place.screen.as_deref().unwrap_or("default"),
            if plan.sidebar { "open" } else { "collapsed" },
            plan.tabs.len(),
            plan.current + 1,
        );
        self.set_sidebar(plan.sidebar, false, cx);
        for (index, tab) in plan.tabs.iter().enumerate() {
            self.restore_tab(number, index + 1, tab, window, cx);
        }
        // A local tab that showed another panel gets it back (a panel needs its tab to be current for a moment).
        for (index, tab) in plan.tabs.iter().enumerate() {
            if tab.wait.is_none() && tab.panel != Panel::Terminal && index != plan.current {
                self.activate(index, window, cx);
                self.show_panel(tab.panel, window, cx);
            }
        }
        let current = plan.current.min(self.sessions.len().saturating_sub(1));
        // The current tab of a remote kind connects now: showing it is what starts the connection.
        self.activate(current, window, cx);
        let tab = &plan.tabs[current.min(plan.tabs.len() - 1)];
        if tab.wait.is_none() && tab.panel != Panel::Terminal {
            self.show_panel(tab.panel, window, cx);
        }
        self.remember_place(window, cx);
    }

    fn restore_tab(
        &mut self,
        window_number: usize,
        number: usize,
        tab: &crate::restore::plan::TabPlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &tab.open {
            Open::Local { cwd } => {
                log::info!(
                    "restore: window {window_number} tab {number}: local shell in {}",
                    cwd.as_deref().unwrap_or("the home directory")
                );
                self.open_local_in(cwd.as_deref().map(PathBuf::from), window, cx);
            }
            Open::Remote { host, quick, dir } => {
                let Some(wait) = tab.wait else { return };
                if *quick {
                    // Like a connection from the quick-connect box: not in the host list, known to this run.
                    self.hub
                        .temporary_hosts
                        .borrow_mut()
                        .insert(host.id.clone(), Host::clone(host));
                }
                let id = match host.protocol {
                    Protocol::Ssh | Protocol::Mosh => {
                        let copies = self
                            .sessions
                            .iter()
                            .filter(|s| s.ssh().is_some_and(|ssh| ssh.host.id == host.id))
                            .count();
                        let title = if copies == 0 {
                            host.name.clone()
                        } else {
                            format!("{} · {}", host.name, copies + 1)
                        };
                        self.push_ssh_session(
                            title,
                            ssh::SshSession::pending(Host::clone(host)),
                            window,
                            cx,
                        )
                    }
                    Protocol::Telnet | Protocol::Serial => {
                        self.push_stream_tab(Host::clone(host), window, cx)
                    }
                };
                log::info!(
                    "restore: window {window_number} tab {number}: {} {:?} ({}), {}",
                    host.protocol.label(),
                    host.name,
                    if *quick {
                        "quick connection"
                    } else {
                        "saved host"
                    },
                    match wait {
                        Wait::Show => "connects when it is shown",
                        Wait::Enter => "waits for Enter",
                    },
                );
                self.apply_terminal_preferences(id, cx);
                if let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) {
                    // Typed into the shell once it is there, the way ⌘⇧T does it; until then the record
                    // of the session keeps the directory too.
                    session.typed = ssh::duplicate_line(Some(host.protocol), dir.as_deref());
                    session.restore_dir = dir.clone().filter(|_| session.typed.is_some());
                    session.pending = Some(Deferred {
                        wait,
                        panel: tab.panel,
                    });
                    session.terminal.update(cx, |terminal, cx| match wait {
                        Wait::Show => terminal.notice(&format!("\x1b[2m{}", wait.notice()), cx),
                        Wait::Enter => terminal.wait_for_enter(wait.notice(), cx),
                    });
                }
            }
        }
    }

    /// Ends the wait of a restored tab that is about to connect. `true` if it was waiting.
    pub(super) fn leave_pending(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        if session.pending.take().is_none() {
            return false;
        }
        // The placeholder line goes; the progress of the connection takes its place. A session log or a
        // recording starts now, not for a tab nobody ever opened.
        session
            .terminal
            .clone()
            .update(cx, |terminal, cx| terminal.wipe(cx));
        self.start_session_archives(id, window, cx);
        true
    }

    /// Something happened to a restored tab that has not connected: it connects if that is what it waits for.
    /// Returns the panel it asks for once the connection is on its way.
    pub(super) fn resume_pending(
        &mut self,
        id: usize,
        event: Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Panel> {
        let session = self.sessions.iter().find(|s| s.id == id)?;
        let Step::Connect { panel } = session.pending.clone()?.on(event) else {
            return None;
        };
        log::info!(
            "restore: connecting tab {id} ({}): {}",
            session.title,
            match event {
                Event::Shown => "it is shown",
                Event::Enter => "Enter was pressed",
            }
        );
        let (ssh, stream) = (
            matches!(session.kind, SessionKind::Ssh(_)),
            matches!(session.kind, SessionKind::Stream(_)),
        );
        if ssh {
            self.connect_pending_ssh(id, window, cx);
        } else if stream {
            self.start_stream(id, window, cx);
        }
        // A tab whose host could not be connected just now is still waiting: it has no panel to show.
        let started = self
            .sessions
            .iter()
            .any(|s| s.id == id && s.pending.is_none());
        (started && panel != Panel::Terminal).then_some(panel)
    }

    /// Enter in a Telnet or serial tab that ended: connect again. A restored serial tab opens only now.
    pub(super) fn restart_stream(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .sessions
            .iter()
            .any(|s| s.id == id && s.pending.is_some())
        {
            self.resume_pending(id, Event::Enter, window, cx);
        } else {
            self.start_stream(id, window, cx);
        }
    }
}

// ---- saving ----------------------------------------------------------------------------------

impl Workspace {
    /// Where this window is now.
    pub(super) fn remember_place(&mut self, window: &Window, cx: &App) {
        let screen = window
            .display(cx)
            .and_then(|display| display.uuid().ok())
            .map(|uuid| uuid.to_string());
        self.located = Some(Located {
            frame: frame_of(window.window_bounds().get_bounds()),
            screen,
        });
    }

    pub(super) fn window_moved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_place(window, cx);
        self.hub.saver.borrow_mut().mark();
    }

    /// What of this window the record holds: the tabs in order, the current one, the panels, the sidebar.
    fn snapshot_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for session in &self.sessions {
            (session.id, session.panel as u8).hash(&mut hasher);
        }
        (self.active, self.sidebar_open).hash(&mut hasher);
        hasher.finish()
    }

    /// Called whenever the window changes: a change in what the record holds marks it for writing.
    pub(super) fn note_snapshot_change(&mut self) {
        let signature = self.snapshot_signature();
        if signature != self.snapshot_sig {
            self.snapshot_sig = signature;
            self.hub.saver.borrow_mut().mark();
        }
    }

    /// The setting was turned on or off: off deletes the saved session, on writes it soon.
    pub(super) fn restore_preference_changed(&mut self, on: bool) {
        self.hub.saver.borrow_mut().set_enabled(on && !demo_run());
        if !on {
            match self.hub.store.borrow_mut().delete_setting(KEY) {
                Ok(()) => log::info!("restore: the setting is off, the saved session was deleted"),
                Err(error) => log::warn!("restore: could not delete the saved session: {error}"),
            }
        }
    }

    fn tab_state(&self, session: &Session, cx: &mut Context<Self>) -> TabState {
        let remote = |host: &shellmate_core::Host, cx: &mut Context<Self>| {
            let saved = self.hub.book.borrow().host(&host.id).is_some();
            // What the shell of the connection it has now reported; the restored directory is `restore_dir`.
            let reported = session.terminal.read(cx).cwd().map(str::to_owned);
            (
                Source::Remote {
                    host: Box::new(host.clone()),
                    saved,
                },
                reported,
            )
        };
        let (source, dir) = match &session.kind {
            SessionKind::Local => {
                // Where the foreground program or the shell is, else where the shell said it was, else where it started.
                let cwd = session.terminal.update(cx, |terminal, _| {
                    terminal
                        .foreground_process()
                        .and_then(|process| process.cwd)
                        .or_else(|| terminal.cwd().map(PathBuf::from))
                        .or_else(|| terminal.options().and_then(|options| options.cwd.clone()))
                });
                (
                    Source::Local {
                        cwd: cwd.and_then(|path| path.to_str().map(str::to_owned)),
                    },
                    None,
                )
            }
            SessionKind::Ssh(ssh) => remote(&ssh.host, cx),
            SessionKind::Stream(stream) => remote(&stream.host, cx),
        };
        TabState {
            source,
            // A tab that has not connected yet shows the terminal, and still has the panel it was restored with.
            panel: session
                .pending
                .as_ref()
                .map_or(session.panel, |pending| pending.panel),
            dir,
            // Kept until the `cd` line has been typed into a connected shell (see `type_pending`).
            restore_dir: session.restore_dir.clone(),
        }
    }

    fn snapshot_state(&self, cx: &mut Context<Self>) -> WindowState {
        WindowState {
            place: self.located.clone(),
            sidebar: self.sidebar_open,
            active: self.active,
            tabs: self
                .sessions
                .iter()
                .map(|session| self.tab_state(session, cx))
                .collect(),
        }
    }
}

impl Hub {
    /// Whether the next launch restores this session (what the confirmations of a quit or an update say).
    pub(crate) fn restores_session(cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|hub| hub.services.saver.borrow().enabled())
    }

    /// Writes the record if something changed and the last write was long enough ago.
    pub(super) fn flush_snapshot(cx: &mut App) {
        if cx
            .global::<Self>()
            .services
            .saver
            .borrow()
            .due(Instant::now())
        {
            Self::write_snapshot(cx);
        }
    }

    /// Writes the record now, whatever the interval: before an update restarts the app.
    pub(crate) fn save_snapshot_now(cx: &mut App) {
        if cx
            .try_global::<Self>()
            .is_some_and(|hub| hub.services.saver.borrow().may_write_now())
        {
            Self::write_snapshot(cx);
        }
    }

    /// The last write, as the app quits. Nothing is written after it: the windows are going away, and a
    /// record of them going away is not the session.
    pub(super) fn save_snapshot_at_quit(cx: &mut App) {
        Self::save_snapshot_now(cx);
        if let Some(hub) = cx.try_global::<Self>() {
            hub.services.saver.borrow_mut().freeze();
        }
    }

    fn write_snapshot(cx: &mut App) {
        let services = cx.global::<Self>().services.clone();
        let mut windows = Vec::new();
        for entry in Self::windows(cx).to_vec() {
            if let Some(workspace) = entry.workspace.upgrade() {
                windows.push(workspace.update(cx, |workspace, cx| workspace.snapshot_state(cx)));
            }
        }
        let snapshot = snapshot::build(&windows);
        let json = snapshot.to_json();
        let written = services.store.borrow_mut().set_setting(KEY, &json);
        let now = Instant::now();
        match written {
            Ok(()) => {
                services.saver.borrow_mut().written(now);
                log::info!(
                    "restore: snapshot written: {} window(s), {} tab(s), {} bytes",
                    snapshot.windows.len(),
                    snapshot.tab_count(),
                    json.len()
                );
            }
            Err(error) => {
                services.saver.borrow_mut().failed(now);
                log::warn!("restore: could not save the session: {error}");
            }
        }
        #[cfg(debug_assertions)]
        if let Some(path) = std::env::var_os("SHELLMATE_DEBUG_SNAPSHOT_TO")
            && let Err(error) = std::fs::write(&path, snapshot.to_json_pretty())
        {
            log::warn!(
                "restore: cannot write the debug copy {}: {error}",
                Path::new(&path).display()
            );
        }
    }
}

// ---- debug hook ------------------------------------------------------------------------------

/// `SHELLMATE_DEBUG_ACTIVATE_AFTER=<seconds>:<tab index>`: after that long the tab (counted from 0) of the
/// front window is made current through the production `activate`, which is what connects a restored
/// remote tab.
#[cfg(debug_assertions)]
pub(super) fn debug_hook(cx: &mut App) {
    let Ok(spec) = std::env::var("SHELLMATE_DEBUG_ACTIVATE_AFTER") else {
        return;
    };
    let Some((seconds, index)) = parse_activate(&spec) else {
        log::warn!(
            "invalid SHELLMATE_DEBUG_ACTIVATE_AFTER {spec:?}: expected <seconds>:<tab index counted from 0>, e.g. 5:2"
        );
        return;
    };
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_secs(seconds))
            .await;
        cx.update(|cx| {
            Hub::update(cx, move |workspace, window, cx| {
                let count = workspace.sessions.len();
                let Some(title) = workspace.sessions.get(index).map(|s| s.title.clone()) else {
                    log::warn!(
                        "debug activate: the front window has {count} tab(s), no tab {index}"
                    );
                    return;
                };
                log::info!("debug activate: tab {index} of {count} ({title}) in the front window");
                workspace.activate(index, window, cx);
            })
        });
    })
    .detach();
}

#[cfg(debug_assertions)]
fn parse_activate(spec: &str) -> Option<(u64, usize)> {
    let (seconds, index) = spec.split_once(':')?;
    Some((seconds.trim().parse().ok()?, index.trim().parse().ok()?))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn the_activate_hook_reads_seconds_and_a_tab_index() {
        assert_eq!(parse_activate("5:2"), Some((5, 2)));
        assert_eq!(parse_activate(" 0 : 0 "), Some((0, 0)));
        for bad in ["", "5", "x:1", "5:", "-5:1", "5:-1", "5:1.5", "5:1:2"] {
            assert_eq!(parse_activate(bad), None, "{bad:?}");
        }
    }
}
