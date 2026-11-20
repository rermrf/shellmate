//! Workspace visibility and application-wide quit coordination.
use super::{Hub, SessionKind, Workspace, forward::ForwardImpact, ssh::SshStatus};
use crate::dialogs::{quit::QuitDialog, window::{Choice, WindowCloseDialog}};
use gpui::{App, Context, WeakEntity, Window};
use shellmate_store::{Preference, Preferences};

#[cfg(target_os = "macos")]
pub(crate) fn native_window(window: &Window) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // GPUI supplies a live NSView for the duration of the Window borrow on the UI thread.
    let view = unsafe { handle.ns_view.cast::<objc2_app_kit::NSView>().as_ref() };
    view.window()
}

#[derive(Debug, PartialEq, Eq)]
enum Close {
    HideInDock,
    HideAsAccessory,
    Quit,
}

fn close_behavior(prefs: &Preferences) -> Close {
    if prefs.get(Preference::QuitOnWindowClose) {
        Close::Quit
    } else if prefs.get(Preference::ShowStatusIcon) {
        Close::HideAsAccessory
    } else {
        Close::HideInDock
    }
}

#[derive(Debug, PartialEq, Eq)]
enum WindowClose {
    Last,
    Confirm,
    Remove,
}

fn window_close(windows: usize, sessions: usize) -> WindowClose {
    match (windows, sessions) {
        (0 | 1, _) => WindowClose::Last,
        (_, 0) => WindowClose::Remove,
        _ => WindowClose::Confirm,
    }
}

/// The tabs and forwarding impact the user actually saw in the close dialog.
struct CloseScope {
    tabs: Vec<usize>,
    forwards: ForwardImpact,
}

impl CloseScope {
    fn confirm(self, handle: gpui::AnyWindowHandle, choice: Choice, cx: &mut App) {
        let Some(entry) = Hub::entry(handle, cx) else { return };
        // A close of a secondary window cannot authorize quitting the application.
        if Hub::windows(cx).len() < 2 {
            return;
        }
        let move_tabs = entry.update(cx, |workspace, window, cx| {
            let same_tabs = workspace.sessions.iter().map(|s| s.id).eq(self.tabs.iter().copied());
            let increased_impact = choice == Choice::Disconnect && {
                let current = workspace.forward_impact(|_| true, cx);
                current.rules > self.forwards.rules || current.connections > self.forwards.connections
            };
            if !same_tabs || increased_impact {
                workspace.close_window(window, cx);
                return None;
            }
            if let Some((_, editor)) = &workspace.editor {
                editor.update(cx, |editor, cx| editor.close_then(window, cx, move |cx| {
                    self.confirm(handle, choice, cx);
                }));
                return None;
            }
            if choice == Choice::Move {
                return Some(self.tabs);
            }
            for id in self.tabs {
                workspace.close_session(id, window, cx);
            }
            None
        });
        if let Some(tabs) = move_tabs.flatten() {
            super::transfer::move_to_recent_window(handle, tabs, cx);
        }
    }
}

#[cfg(target_os = "macos")]
fn set_policy(policy: gpui::ActivationPolicy, cx: &mut App) -> bool {
    use gpui::ActivationPolicy;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let app = NSApplication::sharedApplication(objc2::MainThreadMarker::new().expect("UI thread"));
    let native = match policy {
        ActivationPolicy::Regular => NSApplicationActivationPolicy::Regular,
        ActivationPolicy::Accessory => NSApplicationActivationPolicy::Accessory,
    };
    if app.activationPolicy() == native {
        return false;
    }
    // GPUI keeps its own policy for windowing requests and forwards the change to AppKit.
    cx.set_activation_policy(policy);
    if app.activationPolicy() == native {
        log::info!("activation policy: {policy:?}");
        true
    } else {
        log::warn!("AppKit declined activation policy {policy:?}");
        false
    }
}

#[cfg(target_os = "macos")]
fn has_shellmate_main_menu() -> bool {
    use objc2_foundation::ns_string;
    objc2_app_kit::NSApplication::sharedApplication(objc2::MainThreadMarker::new().expect("UI thread"))
        .mainMenu()
        .and_then(|menu| menu.itemWithTitle(ns_string!("会话")))
        .and_then(|item| item.submenu())
        .is_some_and(|menu| menu.itemWithTitle(ns_string!("新建本机终端")).is_some())
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct Activity {
    pub(super) sessions: usize,
    pub(super) transfers: usize,
    pub(super) forwards: usize,
}

impl Activity {
    fn across_windows(sessions: impl IntoIterator<Item=usize>, transfers: usize, forwards: usize) -> Self {
        Self { sessions: sessions.into_iter().sum(), transfers, forwards }
    }
    fn should_confirm(&self, enabled: bool) -> bool {
        enabled && *self != Self::default()
    }

    fn message(&self) -> String {
        let counts = [
            (self.sessions, "会话"),
            (self.transfers, "文件传输"),
            (self.forwards, "端口转发"),
        ];
        let parts: Vec<_> = counts
            .into_iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, label)| format!("{n} 个{label}"))
            .collect();
        format!("有{}正在进行，退出后会全部断开。", parts.join("、"))
    }
}

impl Workspace {
    #[cfg(target_os = "macos")]
    pub(super) fn sync_hidden_activation_policy(&self, cx: &mut App) {
        let policy = if self.peers(super::Route::All,cx).iter().all(|e| e.workspace.upgrade().is_none_or(|w| w.read(cx).hidden)) && close_behavior(&self.hub.preferences.borrow()) == Close::HideAsAccessory {
            gpui::ActivationPolicy::Accessory
        } else {
            gpui::ActivationPolicy::Regular
        };
        set_policy(policy, cx);
    }

    pub(crate) fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match window_close(Hub::windows(cx).len(), self.sessions.len()) {
            WindowClose::Remove => return window.remove_window(),
            WindowClose::Confirm => {
                if self.close_dialog.as_ref().is_none_or(|dialog| !dialog.is_open()) {
                    let connecting = self.sessions.iter().filter(|session| match &session.kind {
                        SessionKind::Ssh(ssh) => matches!(ssh.status, SshStatus::Connecting | SshStatus::Reconnecting(_)),
                        SessionKind::Stream(stream) => stream.retry_in_flight()
                            || stream.snapshot.as_ref().is_some_and(|state| state.retrying),
                        SessionKind::Local => false,
                    }).count();
                    let scope = CloseScope {
                        tabs: self.sessions.iter().map(|s| s.id).collect(),
                        forwards: self.forward_impact(|_| true, cx),
                    };
                    let impact = scope.forwards;
                    let warning = match (impact.rules, impact.connections) {
                        (0, 0) => None,
                        (0, n) => Some(format!("选择「断开并关闭」会断开 {n} 个转发连接。")),
                        (n, 0) => Some(format!("选择「断开并关闭」会停止 {n} 条端口转发。")),
                        (rules, connections) => Some(format!("选择「断开并关闭」会停止 {rules} 条端口转发，并断开 {connections} 个转发连接。")),
                    };
                    let handle = self.handle;
                    self.close_dialog = Some(WindowCloseDialog::open(
                        scope.tabs.len(), connecting, warning,
                        move |choice, cx| cx.defer(move |cx| scope.confirm(handle, choice, cx)),
                        window, cx,
                    ));
                }
                return;
            }
            WindowClose::Last => {}
        }
        if close_behavior(&self.hub.preferences.borrow()) == Close::Quit {
            return self.request_quit(window, cx);
        }
        if self.hidden {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            let Some(native) = native_window(window) else {
                log::error!("could not obtain the main NSWindow");
                return;
            };
            self.remember_focus_actions(window, cx);
            self.set_window_hidden(true, cx);
            self.sync_menus(cx);
            native.orderOut(None);
            self.sync_hidden_activation_policy(cx);
            log::info!("main window hidden");
        }
        #[cfg(not(target_os = "macos"))]
        window.minimize_window();
    }

    pub(super) fn close_empty_window_if_needed(&self, window: &mut Window, cx: &App) -> bool {
        let close = window_close(Hub::windows(cx).len(), self.sessions.len()) == WindowClose::Remove;
        if close {
            window.remove_window();
        }
        close
    }

    pub(crate) fn show_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        Hub::activated(self.handle, cx);
        self.last_menu_state = None;
        #[cfg(target_os = "macos")]
        {
            let Some(native) = native_window(window) else { return };
            let switched = set_policy(gpui::ActivationPolicy::Regular, cx);
            cx.activate(true);
            if native.isMiniaturized() { native.deminiaturize(None); }
            native.makeKeyAndOrderFront(None);
            if switched {
                // AppKit may still be transferring menu-bar ownership after setActivationPolicy.
                // Wait for the next foreground turn, after sync_menus has installed the visible tree.
                cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(std::time::Duration::ZERO).await;
                    this.update_in(cx, |s, _, cx| {
                        if s.hidden || Hub::windows(cx).first().is_none_or(|entry| entry.handle != s.handle) { return; }
                        if !has_shellmate_main_menu() && let Some((state, _)) = s.last_menu_state {
                            cx.defer(move |cx| crate::menus::install(crate::menus::menus(&state), cx));
                        }
                        cx.activate(true);
                        native.makeKeyAndOrderFront(None);
                        if !has_shellmate_main_menu() {
                            log::warn!("Shellmate main menu missing after restoring activation policy");
                        }
                    }).ok();
                }).detach();
            }
        }
        #[cfg(not(target_os = "macos"))]
        window.activate_window();
        if !self.hidden {
            self.sync_menus(cx);
            return;
        }
        self.set_window_hidden(false, cx);
        self.sync_menus(cx);
        self.sync_sidebar(cx);
        self.sync_ai_context(cx);
        window.refresh();
        log::info!("main window shown");
    }

    fn set_window_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        self.hidden = hidden;
        // The component input caret has its own timer independent of AppKit's display link.
        let all_hidden = hidden && self.peers(super::Route::All,cx).iter().all(|e| e.workspace.upgrade().is_none_or(|w| w.read(cx).hidden));
        gpui::base::Theme::global_mut(cx).cursor_blink = if all_hidden {
            gpui::base::input::CursorBlink::steady()
        } else {
            gpui::component::Theme::global(cx).cursor_blink
        };
        for session in &self.sessions {
            session
                .view
                .update(cx, |view, cx| view.set_display_paused(hidden, cx));
        }
        if let Some(records) = self.hub.records_window.borrow().as_ref().filter(|r| r.owner == self.handle).and_then(|r| r.panel()) {
            records.update(cx, |panel, cx| panel.set_window_visible(!hidden, cx));
        }
        self.sync_monitor_activity(cx);
    }

    fn busy_sessions(&self, cx: &mut App) -> usize {
        self
            .sessions
            .iter()
            .filter(|s| match &s.kind {
                SessionKind::Ssh(ssh) => matches!(
                    ssh.status,
                    SshStatus::Connected | SshStatus::Reconnecting(_)
                ),
                SessionKind::Stream(stream) => stream.connected_or_reconnecting(s.exited),
                SessionKind::Local => {
                    !s.exited
                        && s.terminal.update(cx, |t, _| {
                            t.child_pid()
                                .zip(t.foreground_process())
                                .is_some_and(|(shell, fg)| fg.pid != shell)
                        })
                }
            })
            .count()
    }
    fn quit_activity(&self, cx: &mut Context<Self>) -> Activity {
        let mut counts = vec![self.busy_sessions(cx)];
        for entry in self.peers(super::Route::All,cx) {
            if let Some(peer) = entry.workspace.upgrade() {
                counts.push(peer.update(cx, |s,cx| s.busy_sessions(cx)));
            }
        }
        let transfers = self.hub
            .groups.borrow()
            .iter()
            .filter_map(|g| g.link.as_ref())
            .map(|link| link.queue().running_count())
            .sum();
        let forwards = self.hub
            .groups.borrow()
            .iter()
            .filter_map(|g| g.forward.as_ref())
            .map(|f| f.forwarder.listening_rules())
            .sum();
        Activity::across_windows(counts, transfers, forwards)
    }

    pub(crate) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_window(window, cx);
        let activity = self.quit_activity(cx);
        if activity.should_confirm(self.hub.preferences.borrow().get(Preference::ConfirmQuit)) {
            self.confirm_quit(activity, window, cx);
        } else {
            Hub::quit(cx);
        }
    }

    pub(super) fn confirm_quit(
        &mut self,
        activity: Activity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.hub
            .quit_dialog.borrow()
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .is_none()
        {
            *self.hub.quit_dialog.borrow_mut() = Some(QuitDialog::open(
                activity.message(),
                cx.weak_entity(),
                window,
                cx,
            ));
        }
    }
}

#[cfg(debug_assertions)]
pub(crate) fn debug_hooks(cx: &mut App) {
    type Hook = (&'static str, fn(&mut App));
    let hooks: &[Hook] = &[
        ("SHELLMATE_DEBUG_MOVE_TO_NEW_WINDOW_AFTER", |cx| Hub::update(cx, Workspace::move_session_to_new_window)),
        ("SHELLMATE_DEBUG_NEW_WINDOW_AFTER", |cx| Hub::update(cx, Workspace::new_window)),
        ("SHELLMATE_DEBUG_MERGE_BACK_AFTER", super::transfer::merge_back),
        #[cfg(target_os = "macos")]
        ("SHELLMATE_DEBUG_STATUS_CLICK_AFTER", |cx| {
            Hub::update(cx, |s, _, _| s.hub.status_bar.borrow().debug_click())
        }),
        #[cfg(target_os = "macos")]
        ("SHELLMATE_DEBUG_STATUS_MENU_AFTER", |cx| {
            Hub::update(cx, |s, _, _| s.hub.status_bar.borrow().debug_menu())
        }),
        ("SHELLMATE_DEBUG_HIDE_AFTER", |cx| {
            Hub::update(cx, Workspace::close_window)
        }),
        ("SHELLMATE_DEBUG_REOPEN_AFTER", Hub::reopen),
        ("SHELLMATE_DEBUG_QUIT_AFTER", |cx| {
            cx.dispatch_action(&super::Quit)
        }),
    ];
    for &(key, run) in hooks {
        if let Some(seconds) = std::env::var(key).ok().and_then(|v| v.parse::<u64>().ok()) {
            let timer = cx
                .background_executor()
                .timer(std::time::Duration::from_secs(seconds));
            cx.spawn(async move |cx| {
                timer.await;
                cx.update(run);
            })
            .detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonfinal_windows_confirm_only_when_they_have_tabs() {
        assert_eq!(window_close(1, 3), WindowClose::Last);
        assert_eq!(window_close(2, 3), WindowClose::Confirm);
    }

    #[test]
    fn losing_the_last_tab_closes_only_a_nonfinal_window() {
        assert_eq!(window_close(2, 0), WindowClose::Remove);
        assert_eq!(window_close(1, 0), WindowClose::Last);
    }

    #[test]
    fn close_decision_uses_preference() {
        let mut prefs = Preferences::default();
        assert_eq!(close_behavior(&prefs), Close::HideAsAccessory);
        prefs.set(Preference::ShowStatusIcon, false);
        assert_eq!(close_behavior(&prefs), Close::HideInDock);
        prefs.set(Preference::QuitOnWindowClose, true);
        assert_eq!(close_behavior(&prefs), Close::Quit);
        prefs.set(Preference::ShowStatusIcon, true);
        assert_eq!(close_behavior(&prefs), Close::Quit);
    }

    #[test]
    fn quit_activity_counts_each_shared_engine_once() {
        let activity = Activity::across_windows([2, 0, 3], 1, 4);
        assert_eq!((activity.sessions,activity.transfers,activity.forwards),(5,1,4));
    }

    #[test]
    fn confirmation_and_wording_follow_activity() {
        assert!(!Activity::default().should_confirm(true));
        for (sessions, transfers, forwards, text) in [
            (2, 0, 0, "2 个会话"),
            (0, 1, 0, "1 个文件传输"),
            (0, 0, 2, "2 个端口转发"),
            (3, 1, 2, "3 个会话、1 个文件传输、2 个端口转发"),
        ] {
            let activity = Activity {
                sessions,
                transfers,
                forwards,
            };
            assert!(activity.should_confirm(true));
            assert!(!activity.should_confirm(false));
            assert_eq!(
                activity.message(),
                format!("有{text}正在进行，退出后会全部断开。")
            );
        }
    }
}
