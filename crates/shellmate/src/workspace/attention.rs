//! Background alerts: what a session nobody is looking at tells the user. A bell, a notification
//! request (OSC 9, OSC 777) or, for a local session, a command that ran for a long time and ended
//! puts a dot on the tab, a mark on the menu bar icon, a count on the Dock icon and, when Shellmate
//! is not in front, a system notification. Looking at the session takes all of that back.
//!
//! The decisions are plain functions of plain values ([`Alerts::raise`], [`LongCommand::observe`])
//! and are tested as such; the `Workspace` part below only gathers the facts and acts on the result.
use super::*;
use gpui::SystemNotification;
use shellmate_store::{Preference as P, Preferences};
use std::time::{Duration, Instant};

/// A session gets at most one system notification in this time.
pub(super) const NOTIFY_INTERVAL: Duration = Duration::from_secs(10);
/// A command that ran at least this long is worth telling about when it ends in the background.
pub(super) const LONG_COMMAND: Duration = Duration::from_secs(30);

const TAG_PREFIX: &str = "session-";

/// The identity of a session's system notification: the same session replaces its own, and a click
/// carries it back.
pub(crate) fn tag(session: usize) -> String {
    format!("{TAG_PREFIX}{session}")
}

/// The session a notification tag belongs to.
pub(crate) fn session_of_tag(tag: &str) -> Option<usize> {
    let digits = tag.strip_prefix(TAG_PREFIX)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// What happened in the background.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AlertKind {
    Bell,
    /// The program asked for a notification, with or without a headline of its own.
    Notify { title: Option<String>, body: String },
    /// A command of a local session ended after `elapsed`.
    CommandDone { program: String, elapsed: Duration },
}

impl AlertKind {
    fn label(&self) -> &'static str {
        match self {
            Self::Bell => "响铃",
            Self::Notify { .. } => "通知",
            Self::CommandDone { .. } => "命令完成",
        }
    }

    fn headline(&self) -> Option<&str> {
        match self {
            Self::Notify { title, .. } => title.as_deref(),
            _ => None,
        }
    }

    /// The text of the system notification.
    fn text(&self) -> String {
        match self {
            Self::Bell => "需要你的注意".into(),
            Self::Notify { body, .. } => body.clone(),
            Self::CommandDone { program, elapsed } => {
                format!("命令已完成：{program}（用时 {}）", format_elapsed(*elapsed))
            }
        }
    }
}

/// `45 秒`, `2 分 5 秒`.
fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds} 秒")
    } else {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    }
}

/// An alert nobody has looked at yet: what it was and when.
#[derive(Clone, Debug)]
pub(super) struct Mark {
    pub kind: AlertKind,
    pub at: Instant,
}

/// The facts about whether somebody can see a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Presence {
    /// It is its window's current tab, or shown in the split.
    pub shown: bool,
    /// Its window is hidden (closed to the menu bar or the Dock).
    pub window_hidden: bool,
    /// Its window is the active one.
    pub window_active: bool,
    /// Shellmate is the application in front.
    pub app_frontmost: bool,
}

impl Presence {
    /// Somebody is looking at it: shown in a visible, active window of the foreground application.
    pub(super) fn watched(self) -> bool {
        self.shown && !self.window_hidden && self.window_active && self.app_frontmost
    }

    /// A system notification is of use: the user is in another application, or the window is away.
    fn banner_wanted(self) -> bool {
        !self.app_frontmost || self.window_hidden
    }
}

/// The three settings of 设置 › 终端 that decide what is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Settings {
    pub tab_alerts: bool,
    pub system_notifications: bool,
    pub long_command: bool,
}

impl Settings {
    pub(super) fn of(preferences: &Preferences) -> Self {
        Self {
            tab_alerts: preferences.get(P::TabAlerts),
            system_notifications: preferences.get(P::SystemNotifications),
            long_command: preferences.get(P::LongCommandAlert),
        }
    }

    /// System notifications are part of the tab alerts: they go with them.
    fn banners(self) -> bool {
        self.tab_alerts && self.system_notifications
    }

    pub(super) fn long_commands(self) -> bool {
        self.tab_alerts && self.long_command
    }
}

/// What to tell the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Notice {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Raised {
    /// The mark is new or different: something on screen changes.
    pub changed: bool,
    pub notice: Option<Notice>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Cleared {
    pub was_marked: bool,
    /// A system notification may still be showing: withdraw it.
    pub dismiss: bool,
}

/// A session's alert state. It moves with the session from window to window.
#[derive(Debug, Default)]
pub(super) struct Alerts {
    mark: Option<Mark>,
    last_notified: Option<Instant>,
    /// A system notification was posted and not withdrawn since.
    notified: bool,
}

impl Alerts {
    pub(super) fn marked(&self) -> bool {
        self.mark.is_some()
    }

    pub(super) fn mark(&self) -> Option<&Mark> {
        self.mark.as_ref()
    }

    /// Something happened in the session. Nothing changes when it is being watched (there is no
    /// visual bell) or the alerts are off. Otherwise it is marked and, if the user is away and the
    /// last notification is old enough, a system notification is due. `tab_title` heads notices
    /// that bring no headline of their own.
    pub(super) fn raise(
        &mut self,
        kind: AlertKind,
        tab_title: &str,
        presence: Presence,
        settings: Settings,
        now: Instant,
    ) -> Raised {
        if !settings.tab_alerts || presence.watched() {
            return Raised::default();
        }
        // A bell adds nothing to what is already waiting, and a run of them is one.
        if kind == AlertKind::Bell && self.mark.is_some() {
            return Raised::default();
        }
        let changed = self.mark.as_ref().map(|mark| &mark.kind) != Some(&kind);
        let notice = (settings.banners() && presence.banner_wanted() && self.notice_due(now)).then(|| {
            self.last_notified = Some(now);
            self.notified = true;
            Notice { title: kind.headline().unwrap_or(tab_title).to_string(), body: kind.text() }
        });
        if changed {
            self.mark = Some(Mark { kind, at: now });
        }
        Raised { changed, notice }
    }

    fn notice_due(&self, now: Instant) -> bool {
        self.last_notified.is_none_or(|at| now.saturating_duration_since(at) >= NOTIFY_INTERVAL)
    }

    /// The session is being looked at, or was closed.
    pub(super) fn clear(&mut self) -> Cleared {
        Cleared { was_marked: self.mark.take().is_some(), dismiss: std::mem::take(&mut self.notified) }
    }

    /// The system notification is no longer wanted (the setting was turned off); the mark stays.
    pub(super) fn withdraw_notice(&mut self) -> bool {
        std::mem::take(&mut self.notified)
    }
}

/// What the foreground-process poll of a local session saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Seen {
    /// The shell itself, waiting for the next command.
    Shell,
    Program(String),
    /// The poll found nothing out (the process list could not be read).
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Finished {
    pub program: String,
    pub elapsed: Duration,
}

/// Follows what a local session runs in the foreground, to notice a long command ending. The poll
/// is the workspace's one-second tick, so the time is right to about a second.
#[derive(Debug, Default)]
pub(super) struct LongCommand {
    running: Option<(String, Instant)>,
}

impl LongCommand {
    /// `seen` at `now`. Returns the command that was in the foreground, ran at least
    /// [`LONG_COMMAND`] since it was first seen, and has now given the terminal back to the shell.
    pub(super) fn observe(&mut self, now: Instant, seen: Seen) -> Option<Finished> {
        match seen {
            Seen::Unknown => None,
            Seen::Program(name) => {
                if self.running.as_ref().is_none_or(|(running, _)| *running != name) {
                    self.running = Some((name, now));
                }
                None
            }
            Seen::Shell => {
                let (program, since) = self.running.take()?;
                let elapsed = now.saturating_duration_since(since);
                (elapsed >= LONG_COMMAND).then_some(Finished { program, elapsed })
            }
        }
    }

    pub(super) fn reset(&mut self) {
        self.running = None;
    }
}

impl Workspace {
    pub(super) fn alert_settings(&self) -> Settings {
        Settings::of(&self.hub.preferences.borrow())
    }

    fn is_shown(&self, id: usize) -> bool {
        let main = self.active.and_then(|ix| self.sessions.get(ix)).map(|s| (s.id, s.panel));
        on_screen(main, self.split.other, id)
    }

    pub(super) fn presence(&self, id: usize, window: &Window) -> Presence {
        let window_active = window.is_window_active();
        Presence {
            shown: self.is_shown(id),
            window_hidden: self.hidden,
            window_active,
            app_frontmost: window_active || self.hub.app_frontmost(),
        }
    }

    /// Something happened in the background of session `id`.
    pub(super) fn raise_alert(&mut self, id: usize, kind: AlertKind, window: &Window, cx: &mut Context<Self>) {
        let settings = self.alert_settings();
        let presence = self.presence(id, window);
        let Some(ix) = self.sessions.iter().position(|s| s.id == id) else { return };
        let title = self.sessions[ix].display_title(cx);
        let label = kind.label();
        let raised = self.sessions[ix].alerts.raise(kind, &title, presence, settings, Instant::now());
        if raised.changed || raised.notice.is_some() {
            log::info!(
                "alert: session {id} ({title}) {label}; watched={} app_frontmost={} window_hidden={} -> mark={} system_notification={}",
                presence.watched(), presence.app_frontmost, presence.window_hidden, raised.changed, raised.notice.is_some(),
            );
        }
        if let Some(notice) = raised.notice {
            let tag = tag(id);
            log::info!("system notification: tag={tag} title={:?} body={:?}", notice.title, notice.body);
            cx.show_system_notification(SystemNotification {
                tag: tag.into(),
                title: notice.title.into(),
                body: notice.body.into(),
                actions: Vec::new(),
            });
        }
        if raised.changed {
            cx.notify();
        }
    }

    /// The user looks at session `id` (or it is gone): its mark and system notification go away.
    pub(super) fn clear_alerts(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) else { return };
        let marked = session.alerts.mark().map(|mark| (mark.kind.label(), mark.at));
        let cleared = session.alerts.clear();
        if cleared.dismiss {
            cx.dismiss_system_notification(&tag(id));
        }
        if let Some((label, at)) = marked {
            log::info!("alert: session {id} {label} cleared after {:.1}s", at.elapsed().as_secs_f32());
        }
        if cleared.was_marked {
            cx.notify();
        }
    }

    /// Takes the alerts of every session that is being looked at now.
    pub(super) fn clear_watched_alerts(&mut self, window: &Window, cx: &mut Context<Self>) {
        let seen: Vec<usize> = self
            .sessions
            .iter()
            .filter(|s| s.alerts.marked() && self.presence(s.id, window).watched())
            .map(|s| s.id)
            .collect();
        for id in seen {
            self.clear_alerts(id, cx);
        }
    }

    /// One of the three alert settings changed: what no longer applies goes away at once.
    pub(super) fn alert_settings_changed(&mut self, cx: &mut Context<Self>) {
        let settings = self.alert_settings();
        let ids: Vec<usize> = self.sessions.iter().map(|s| s.id).collect();
        for id in ids {
            if !settings.tab_alerts {
                self.clear_alerts(id, cx);
            } else if !settings.system_notifications
                && self.sessions.iter_mut().find(|s| s.id == id).is_some_and(|s| s.alerts.withdraw_notice())
            {
                cx.dismiss_system_notification(&tag(id));
            }
        }
        if !settings.long_commands() {
            for session in &mut self.sessions {
                session.long.reset();
            }
        }
        cx.notify();
    }

    /// Sessions of this window that carry a mark.
    pub(super) fn alert_count(&self) -> usize {
        self.sessions.iter().filter(|s| s.alerts.marked()).count()
    }

    /// Dock badge: how many sessions of all windows wait for a look (nothing at zero).
    #[cfg(target_os = "macos")]
    pub(super) fn sync_dock_badge(&self, cx: &App) {
        let total = self.alert_count()
            + self.peers(Route::All, cx).iter().filter_map(|e| e.workspace.upgrade()).map(|w| w.read(cx).alert_count()).sum::<usize>();
        if self.hub.dock_badge.replace(total) != total {
            set_dock_badge(total);
        }
    }

    /// Work of the one-second tick: a long command that ended, and alerts of sessions that are
    /// being looked at by now (the events that make a session watched clear them at once; this is
    /// for the rest, like a split that opened).
    pub(super) fn tick_attention(&mut self, window: &Window, cx: &mut Context<Self>) {
        let settings = self.alert_settings();
        let now = Instant::now();
        let mut finished = Vec::new();
        for session in &mut self.sessions {
            let seen = if settings.long_commands() && matches!(session.kind, SessionKind::Local) {
                session.terminal.update(cx, |terminal, _| foreground_seen(terminal))
            } else {
                None
            };
            let Some(seen) = seen else {
                session.long.reset();
                continue;
            };
            if let Some(done) = session.long.observe(now, seen) {
                finished.push((session.id, done));
            }
        }
        for (id, Finished { program, elapsed }) in finished {
            self.raise_alert(id, AlertKind::CommandDone { program, elapsed }, window, cx);
        }
        self.clear_watched_alerts(window, cx);
    }

    /// Tells every terminal whether it has the focus, which is what focus reporting (`?1004h`)
    /// follows: the terminal is the focused element of the active window of the foreground
    /// application, and the window is not hidden. A terminal reports changes only.
    pub(super) fn sync_terminal_focus(&self, window: &Window, cx: &mut Context<Self>) {
        for session in &self.sessions {
            let focused = has_focus(self.hidden, window.is_window_active(), session.view.focus_handle(cx).is_focused(window));
            session.terminal.update(cx, |terminal, cx| terminal.set_focused(focused, cx));
        }
    }
}

/// Whether session `id` is on screen in a window: it is the window's current tab (`main`, with the
/// panel it shows), or the other half of the split, which is shown next to a terminal only.
fn on_screen(main: Option<(usize, Panel)>, split_other: Option<usize>, id: usize) -> bool {
    match main {
        Some((main, panel)) => main == id || (panel == Panel::Terminal && split_other == Some(id)),
        None => false,
    }
}

/// A terminal has the focus when it is the focused element of the active window, and the window is
/// not hidden. (The active window is the one of the application in front.)
fn has_focus(window_hidden: bool, window_active: bool, view_focused: bool) -> bool {
    !window_hidden && window_active && view_focused
}

/// What the foreground of a local session is; `None` when it cannot be known (an ended session, or a
/// terminal that does not own a shell).
fn foreground_seen(terminal: &mut Terminal) -> Option<Seen> {
    let options = terminal.options()?.clone();
    if terminal.is_exited() {
        return None;
    }
    let shell = shell_name(&options.shell_path());
    Some(match terminal.foreground_process() {
        Some(process) if process.name == shell => Seen::Shell,
        Some(process) => Seen::Program(process.name),
        None => Seen::Unknown,
    })
}

#[cfg(target_os = "macos")]
fn set_dock_badge(count: usize) {
    use objc2_foundation::NSString;
    let Some(mtm) = objc2::MainThreadMarker::new() else { return };
    let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
    objc2_app_kit::NSApplication::sharedApplication(mtm).dockTile().setBadgeLabel(label.as_deref());
    log::info!("dock badge: {}", if count > 0 { count.to_string() } else { "none".into() });
}

impl Hub {
    /// Brings session `id` to the front: its window is shown and its tab selected, and what waited
    /// in it has been seen. With `terminal`, a tab that shows another panel goes back to its terminal.
    /// `false` when no window has the session (it was closed).
    pub(crate) fn show_session(id: usize, terminal: bool, cx: &mut App) -> bool {
        let Some(entry) = Route::Session(id).target(cx) else { return false };
        entry.update(cx, move |workspace, window, cx| {
            workspace.show_window(window, cx);
            if let Some(ix) = workspace.sessions.iter().position(|tab| tab.id == id) {
                workspace.activate(ix, window, cx);
                if terminal && workspace.sessions[ix].panel != Panel::Terminal {
                    workspace.show_panel(Panel::Terminal, window, cx);
                }
                workspace.clear_alerts(id, cx);
            }
        });
        true
    }

    /// The user clicked a system notification. An alert of a session that is gone only brings
    /// the application forward.
    pub(super) fn notification_clicked(tag: &str, cx: &mut App) {
        log::info!("system notification clicked: {tag}");
        let Some(id) = session_of_tag(tag) else { return };
        if !Self::show_session(id, true, cx) {
            cx.activate(true);
        }
    }
}

impl Services {
    pub(super) fn set_window_active(&self, window: gpui::WindowId, active: bool) {
        let mut windows = self.active_windows.borrow_mut();
        if active {
            windows.insert(window);
        } else {
            windows.remove(&window);
        }
    }

    /// Shellmate is the application in front when one of its windows is the active window.
    pub(super) fn app_frontmost(&self) -> bool {
        !self.active_windows.borrow().is_empty()
    }
}

/// Debug scenes: two sessions the user has not looked at, as if their programs had rung the bell
/// and asked for a notification while another tab was in front.
#[cfg(debug_assertions)]
impl Workspace {
    pub(super) fn proto_alerts(&mut self, cx: &mut Context<Self>) {
        let settings = self.alert_settings();
        let in_view = Presence { shown: false, window_hidden: false, window_active: true, app_frontmost: true };
        let waiting = [
            (1, AlertKind::Bell),
            (4, AlertKind::Notify { title: None, body: "构建完成".into() }),
        ];
        for (ix, kind) in waiting {
            if let Some(session) = self.sessions.get_mut(ix) {
                session.alerts.raise(kind, "", in_view, settings, Instant::now());
            }
        }
        cx.notify();
    }
}

/// `SHELLMATE_DEBUG_NOTIFY_AFTER=<seconds>`: after that long a second local tab is opened and the
/// first one comes back to the front. Then the second tab's shell is told to print an OSC 9
/// sequence, so that the whole chain runs as in real use: scanner, event, alert, system notification.
/// The log shows what each step decided.
#[cfg(debug_assertions)]
pub(super) fn debug_notify(cx: &mut App) {
    Hub::update(cx, |workspace, window, cx| {
        if workspace.sessions.is_empty() {
            workspace.open_local(window, cx);
        }
        let first = workspace.active.unwrap_or(0);
        workspace.open_local(window, cx);
        let Some((id, terminal)) = workspace.sessions.last().map(|s| (s.id, s.terminal.clone())) else { return };
        workspace.activate(first, window, cx);
        log::info!("notify hook: session {id} is in the background; asking its shell for an OSC 9 notification");
        cx.spawn(async move |_, cx| {
            // Let the new shell come up before it is told what to print.
            cx.background_executor().timer(Duration::from_millis(2500)).await;
            terminal.update(cx, |terminal, _| terminal.input(b"printf '\\033]9;debug-notify\\007'\r"));
        })
        .detach();
    });
}

/// Logs each focus report when `SHELLMATE_DEBUG_FOCUS_LOG` is set.
#[cfg(debug_assertions)]
pub(super) fn log_focus_report(id: usize, title: &str, focused: bool) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("SHELLMATE_DEBUG_FOCUS_LOG").is_some_and(|v| v != "0")) {
        log::info!("focus report: session {id} ({title}) -> {}", if focused { "in  (ESC [ I)" } else { "out (ESC [ O)" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence(shown: bool, window_hidden: bool, window_active: bool, app_frontmost: bool) -> Presence {
        Presence { shown, window_hidden, window_active, app_frontmost }
    }

    /// In front, in the active window, on its tab.
    const WATCHED: Presence = Presence { shown: true, window_hidden: false, window_active: true, app_frontmost: true };
    /// The user is in another application.
    const AWAY: Presence = Presence { shown: true, window_hidden: false, window_active: false, app_frontmost: false };
    /// Another tab of the active window is open.
    const OTHER_TAB: Presence = Presence { shown: false, ..WATCHED };
    const ALL_ON: Settings = Settings { tab_alerts: true, system_notifications: true, long_command: true };

    fn at(seconds: u64) -> Instant {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *START.get_or_init(Instant::now) + Duration::from_secs(seconds)
    }

    fn notify(text: &str) -> AlertKind {
        AlertKind::Notify { title: None, body: text.into() }
    }

    #[test]
    fn a_session_is_watched_only_when_every_condition_holds() {
        for bits in 0..16u8 {
            let [shown, hidden, active, frontmost] = [0, 1, 2, 3].map(|bit| bits >> bit & 1 == 1);
            let expected = shown && !hidden && active && frontmost;
            assert_eq!(presence(shown, hidden, active, frontmost).watched(), expected, "{bits:04b}");
        }
        assert!(WATCHED.watched());
        assert!(!OTHER_TAB.watched());
        assert!(!AWAY.watched());
    }

    #[test]
    fn a_system_notification_is_wanted_only_when_the_user_is_away_or_the_window_is() {
        assert!(!WATCHED.banner_wanted());
        assert!(!OTHER_TAB.banner_wanted(), "in Shellmate, only the dot shows");
        assert!(AWAY.banner_wanted());
        // Another Shellmate window is in front: still in the application.
        assert!(!presence(true, false, false, true).banner_wanted());
        // The window is hidden: the user cannot see a dot.
        assert!(presence(true, true, false, true).banner_wanted());
        assert!(presence(false, true, true, true).banner_wanted());
    }

    #[test]
    fn a_session_is_on_screen_as_the_tab_or_as_the_other_half_of_a_terminal_split() {
        let tab = |id, panel| Some((id, panel));
        assert!(on_screen(tab(3, Panel::Terminal), None, 3));
        assert!(on_screen(tab(3, Panel::Files), None, 3), "the tab is current whatever panel it shows");
        assert!(!on_screen(tab(3, Panel::Terminal), None, 4), "another tab");
        assert!(on_screen(tab(3, Panel::Terminal), Some(4), 4), "the split shows it");
        assert!(!on_screen(tab(3, Panel::Terminal), Some(4), 5));
        assert!(!on_screen(tab(3, Panel::Files), Some(4), 4), "the split is only drawn with the terminal");
        assert!(!on_screen(None, Some(4), 4), "no tab, no window content");
        assert!(!on_screen(None, None, 3));
    }

    #[test]
    fn a_terminal_has_the_focus_in_the_active_visible_window_only() {
        assert!(has_focus(false, true, true));
        assert!(!has_focus(false, true, false), "the keys go to something else in the window");
        assert!(!has_focus(false, false, true), "another window, or another application, is active");
        assert!(!has_focus(true, true, true), "a hidden window");
        assert!(!has_focus(true, false, false));
    }

    #[test]
    fn what_is_being_watched_raises_nothing() {
        let mut alerts = Alerts::default();
        for kind in [AlertKind::Bell, notify("x"), AlertKind::CommandDone { program: "make".into(), elapsed: at(40) - at(0) }] {
            assert_eq!(alerts.raise(kind, "tab", WATCHED, ALL_ON, at(0)), Raised::default());
        }
        assert!(!alerts.marked());
    }

    #[test]
    fn a_bell_in_the_background_marks_the_tab() {
        let mut alerts = Alerts::default();
        let raised = alerts.raise(AlertKind::Bell, "prod-web", OTHER_TAB, ALL_ON, at(5));
        assert!(raised.changed);
        assert_eq!(raised.notice, None, "the user is in Shellmate: no system notification");
        let mark = alerts.mark().unwrap();
        assert_eq!((&mark.kind, mark.at), (&AlertKind::Bell, at(5)));
    }

    #[test]
    fn a_system_notification_says_what_happened_under_the_tab_title() {
        let mut alerts = Alerts::default();
        let raised = alerts.raise(AlertKind::Bell, "prod-web", AWAY, ALL_ON, at(0));
        assert_eq!(raised.notice, Some(Notice { title: "prod-web".into(), body: "需要你的注意".into() }));

        let mut alerts = Alerts::default();
        let raised = alerts.raise(notify("build finished"), "本机", AWAY, ALL_ON, at(0));
        assert_eq!(raised.notice, Some(Notice { title: "本机".into(), body: "build finished".into() }));

        let titled = AlertKind::Notify { title: Some("Backup".into()), body: "done".into() };
        let mut alerts = Alerts::default();
        let raised = alerts.raise(titled, "本机", AWAY, ALL_ON, at(0));
        assert_eq!(raised.notice, Some(Notice { title: "Backup".into(), body: "done".into() }));

        let done = AlertKind::CommandDone { program: "cargo".into(), elapsed: Duration::from_secs(125) };
        let mut alerts = Alerts::default();
        let raised = alerts.raise(done, "app — zsh", AWAY, ALL_ON, at(0));
        assert_eq!(raised.notice, Some(Notice { title: "app — zsh".into(), body: "命令已完成：cargo（用时 2 分 5 秒）".into() }));
    }

    #[test]
    fn a_hidden_window_gets_the_system_notification_even_in_front() {
        let hidden = presence(true, true, false, true);
        let mut alerts = Alerts::default();
        assert!(alerts.raise(AlertKind::Bell, "t", hidden, ALL_ON, at(0)).notice.is_some());
    }

    #[test]
    fn at_most_one_system_notification_per_ten_seconds() {
        let mut alerts = Alerts::default();
        assert!(alerts.raise(notify("one"), "t", AWAY, ALL_ON, at(0)).notice.is_some());
        // Different content, still the same session: the interval holds, the mark follows.
        let second = alerts.raise(notify("two"), "t", AWAY, ALL_ON, at(9));
        assert_eq!((second.changed, second.notice), (true, None));
        assert_eq!(alerts.mark().unwrap().kind, notify("two"));
        let third = alerts.raise(notify("three"), "t", AWAY, ALL_ON, at(10));
        assert_eq!(third.notice, Some(Notice { title: "t".into(), body: "three".into() }));
        // The interval counts from the last one actually posted.
        assert!(alerts.raise(notify("four"), "t", AWAY, ALL_ON, at(19)).notice.is_none());
        assert!(alerts.raise(notify("five"), "t", AWAY, ALL_ON, at(20)).notice.is_some());
    }

    #[test]
    fn the_interval_survives_a_look_at_the_session() {
        let mut alerts = Alerts::default();
        assert!(alerts.raise(notify("one"), "t", AWAY, ALL_ON, at(0)).notice.is_some());
        assert_eq!(alerts.clear(), Cleared { was_marked: true, dismiss: true });
        assert!(alerts.raise(notify("two"), "t", AWAY, ALL_ON, at(4)).notice.is_none());
        assert!(alerts.marked());
    }

    #[test]
    fn a_run_of_bells_is_one_alert() {
        let mut alerts = Alerts::default();
        assert!(alerts.raise(AlertKind::Bell, "t", AWAY, ALL_ON, at(0)).notice.is_some());
        for second in 1..40 {
            assert_eq!(alerts.raise(AlertKind::Bell, "t", AWAY, ALL_ON, at(second)), Raised::default(), "{second}");
        }
        assert_eq!(alerts.mark().unwrap().at, at(0));
    }

    #[test]
    fn a_notification_request_replaces_a_waiting_bell_and_an_equal_one_changes_nothing() {
        let mut alerts = Alerts::default();
        alerts.raise(AlertKind::Bell, "t", OTHER_TAB, ALL_ON, at(0));
        let raised = alerts.raise(notify("hello"), "t", OTHER_TAB, ALL_ON, at(1));
        assert!(raised.changed);
        assert_eq!(alerts.mark().unwrap().kind, notify("hello"));
        let again = alerts.raise(notify("hello"), "t", OTHER_TAB, ALL_ON, at(2));
        assert_eq!(again, Raised::default(), "nothing on screen would change");
        assert_eq!(alerts.mark().unwrap().at, at(1));
        // A bell does not take the message away.
        alerts.raise(AlertKind::Bell, "t", OTHER_TAB, ALL_ON, at(3));
        assert_eq!(alerts.mark().unwrap().kind, notify("hello"));
    }

    #[test]
    fn looking_at_the_session_clears_the_mark_and_withdraws_the_notification() {
        let mut alerts = Alerts::default();
        assert_eq!(alerts.clear(), Cleared { was_marked: false, dismiss: false });
        alerts.raise(AlertKind::Bell, "t", OTHER_TAB, ALL_ON, at(0));
        assert_eq!(alerts.clear(), Cleared { was_marked: true, dismiss: false }, "no notification was posted");
        alerts.raise(notify("x"), "t", AWAY, ALL_ON, at(20));
        assert_eq!(alerts.clear(), Cleared { was_marked: true, dismiss: true });
        assert_eq!(alerts.clear(), Cleared { was_marked: false, dismiss: false });
        assert!(!alerts.marked());
    }

    #[test]
    fn the_settings_switch_the_parts_off() {
        let off = |tab_alerts, system_notifications| Settings { tab_alerts, system_notifications, long_command: true };
        let mut alerts = Alerts::default();
        assert_eq!(alerts.raise(AlertKind::Bell, "t", AWAY, off(false, true), at(0)), Raised::default());
        assert_eq!(alerts.raise(AlertKind::Bell, "t", AWAY, off(false, false), at(0)), Raised::default());
        assert!(!alerts.marked(), "without tab alerts nothing is marked, and system notifications go with them");
        let quiet = alerts.raise(AlertKind::Bell, "t", AWAY, off(true, false), at(0));
        assert_eq!((quiet.changed, quiet.notice), (true, None));
        assert!(alerts.marked());
        assert_eq!(alerts.clear(), Cleared { was_marked: true, dismiss: false });
        assert!(off(true, true).banners() && !off(true, false).banners() && !off(false, true).banners());
    }

    #[test]
    fn turning_notifications_off_withdraws_the_one_showing_but_keeps_the_mark() {
        let mut alerts = Alerts::default();
        alerts.raise(notify("x"), "t", AWAY, ALL_ON, at(0));
        assert!(alerts.withdraw_notice());
        assert!(!alerts.withdraw_notice());
        assert!(alerts.marked());
        assert_eq!(alerts.clear(), Cleared { was_marked: true, dismiss: false });
    }

    #[test]
    fn the_long_command_setting_depends_on_the_tab_alerts() {
        assert!(ALL_ON.long_commands());
        assert!(!Settings { tab_alerts: false, ..ALL_ON }.long_commands());
        assert!(!Settings { long_command: false, ..ALL_ON }.long_commands());
    }

    #[test]
    fn settings_are_read_from_the_preferences() {
        let mut preferences = Preferences::default();
        assert_eq!(Settings::of(&preferences), ALL_ON, "all three are on by default");
        preferences.set(P::SystemNotifications, false);
        preferences.set(P::LongCommandAlert, false);
        assert_eq!(Settings::of(&preferences), Settings { tab_alerts: true, system_notifications: false, long_command: false });
    }

    #[test]
    fn elapsed_time_reads_in_seconds_then_minutes() {
        for (seconds, text) in [(30, "30 秒"), (59, "59 秒"), (60, "1 分 0 秒"), (125, "2 分 5 秒"), (3725, "62 分 5 秒")] {
            assert_eq!(format_elapsed(Duration::from_secs(seconds)), text);
        }
        assert_eq!(format_elapsed(Duration::from_millis(31_900)), "31 秒");
    }

    #[test]
    fn tags_round_trip_and_foreign_ones_are_refused() {
        for id in [1, 12, 4096, usize::MAX] {
            assert_eq!(session_of_tag(&tag(id)), Some(id));
        }
        assert_eq!(tag(7), "session-7");
        for bad in ["", "session-", "session-x", "session-1x", "session--1", "session-+3", "Session-3", "x-session-3", "session-3 ", "session-99999999999999999999999"] {
            assert_eq!(session_of_tag(bad), None, "{bad:?}");
        }
    }

    fn program(name: &str) -> Seen {
        Seen::Program(name.into())
    }

    #[test]
    fn a_command_that_ran_thirty_seconds_is_reported_when_the_shell_is_back() {
        let mut watch = LongCommand::default();
        assert_eq!(watch.observe(at(0), Seen::Shell), None);
        assert_eq!(watch.observe(at(1), program("cargo")), None);
        for second in 2..31 {
            assert_eq!(watch.observe(at(second), program("cargo")), None);
        }
        let done = watch.observe(at(31), Seen::Shell).unwrap();
        assert_eq!(done, Finished { program: "cargo".into(), elapsed: Duration::from_secs(30) });
        assert_eq!(watch.observe(at(32), Seen::Shell), None, "reported once");
    }

    #[test]
    fn a_shorter_command_is_not_worth_it() {
        let mut watch = LongCommand::default();
        watch.observe(at(0), program("make"));
        assert_eq!(watch.observe(at(29), Seen::Shell), None);
        // Seen for 29 s and then gone: the next command starts from its own first sighting.
        watch.observe(at(40), program("make"));
        assert_eq!(watch.observe(at(69), Seen::Shell), None);
        watch.observe(at(70), program("make"));
        assert!(watch.observe(at(100), Seen::Shell).is_some());
    }

    #[test]
    fn a_failed_poll_changes_nothing() {
        let mut watch = LongCommand::default();
        watch.observe(at(0), program("ssh"));
        assert_eq!(watch.observe(at(20), Seen::Unknown), None);
        assert_eq!(watch.observe(at(40), Seen::Unknown), None);
        let done = watch.observe(at(45), Seen::Shell).unwrap();
        assert_eq!((done.program.as_str(), done.elapsed.as_secs()), ("ssh", 45));
        // Nothing is reported for a prompt that never ran anything.
        let mut idle = LongCommand::default();
        for second in 0..100 {
            assert_eq!(idle.observe(at(second), Seen::Shell), None);
        }
    }

    #[test]
    fn another_program_in_front_restarts_the_clock() {
        let mut watch = LongCommand::default();
        watch.observe(at(0), program("less"));
        watch.observe(at(25), program("vim"));
        assert_eq!(watch.observe(at(40), Seen::Shell), None, "vim ran 15 s");
        watch.observe(at(50), program("less"));
        watch.observe(at(60), program("less"));
        let done = watch.observe(at(95), Seen::Shell).unwrap();
        assert_eq!((done.program.as_str(), done.elapsed.as_secs()), ("less", 45));
    }

    #[test]
    fn a_reset_forgets_the_command() {
        let mut watch = LongCommand::default();
        watch.observe(at(0), program("make"));
        watch.reset();
        assert_eq!(watch.observe(at(100), Seen::Shell), None);
    }
}
