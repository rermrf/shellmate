//! Workspace visibility and application-wide quit coordination.
use super::{Hub, SessionKind, Workspace, close_guard::CloseImpact, ssh::SshStatus};
use crate::dialogs::{quit::QuitDialog, window::{Choice, WindowCloseDialog}};
use gpui::{App, Context, WeakEntity, Window};
use shellmate_store::{CloseWindowWithSessions, Preference, Preferences};

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
    Move,
    Disconnect,
    Remove,
}

fn window_close(windows: usize, sessions: usize, risky: bool, policy: CloseWindowWithSessions) -> WindowClose {
    match (windows, sessions) {
        (0 | 1, _) => WindowClose::Last,
        (_, 0) => WindowClose::Remove,
        _ => match policy {
            CloseWindowWithSessions::Ask if risky => WindowClose::Confirm,
            CloseWindowWithSessions::Ask => WindowClose::Disconnect,
            CloseWindowWithSessions::Move => WindowClose::Move,
            CloseWindowWithSessions::Disconnect => WindowClose::Disconnect,
        },
    }
}

/// The tabs, jobs and resources the user actually saw in the close dialog.
struct CloseScope {
    tabs: Vec<usize>,
    impact: CloseImpact,
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
            let increased_impact = choice == Choice::Disconnect
                && !workspace.close_impact(&self.tabs, cx).covered_by(&self.impact);
            if !same_tabs || increased_impact {
                // A remembered choice applies to future requests, not to new work
                // that appeared while this particular confirmation was open.
                workspace.close_window_with_policy(CloseWindowWithSessions::Ask, window, cx);
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

impl Workspace {
    pub(crate) fn set_close_window_preference(&mut self, value: CloseWindowWithSessions, cx: &mut Context<Self>) -> Result<(), String> {
        self.hub.store.borrow_mut().set_close_window_with_sessions(value).map_err(|e| e.to_string())?;
        self.hub.preferences.borrow_mut().close_window_with_sessions = value;
        cx.emit(super::CloseWindowPreferenceChanged(value));
        for entry in self.peers(super::Route::All, cx) {
            cx.defer(move |cx| { entry.update(cx, |_, _, cx| cx.emit(super::CloseWindowPreferenceChanged(value))); });
        }
        Ok(())
    }

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
        let policy = self.hub.preferences.borrow().close_window_with_sessions;
        self.close_window_with_policy(policy, window, cx);
    }

    fn close_window_with_policy(&mut self, policy: CloseWindowWithSessions, window: &mut Window, cx: &mut Context<Self>) {
        let tabs: Vec<_> = self.sessions.iter().map(|s| s.id).collect();
        let impact = self.close_impact(&tabs, cx);
        log::debug!("window close: tabs={} needs_confirmation={}", tabs.len(), impact.needs_confirmation());
        match window_close(Hub::windows(cx).len(), self.sessions.len(), impact.needs_confirmation(), policy) {
            WindowClose::Remove => return window.remove_window(),
            WindowClose::Move | WindowClose::Disconnect => {
                let choice = if policy == CloseWindowWithSessions::Move { Choice::Move } else { Choice::Disconnect };
                let scope = CloseScope { tabs, impact };
                let handle = self.handle;
                cx.defer(move |cx| scope.confirm(handle, choice, cx));
                return;
            }
            WindowClose::Confirm => {
                if self.close_dialog.as_ref().is_none_or(|dialog| !dialog.is_open()) {
                    let connecting = self.sessions.iter().filter(|session| match &session.kind {
                        SessionKind::Ssh(ssh) => matches!(ssh.status, SshStatus::Connecting | SshStatus::Reconnecting(_)),
                        SessionKind::Stream(stream) => stream.retry_in_flight()
                            || stream.snapshot.as_ref().is_some_and(|state| state.retrying),
                        SessionKind::Local => false,
                    }).count();
                    let warning = format!("选择「断开并关闭」时：\n{}", impact.message());
                    let scope = CloseScope { tabs, impact };
                    let handle = self.handle;
                    let workspace = cx.weak_entity();
                    let mut scope = Some(scope);
                    self.close_dialog = Some(WindowCloseDialog::open(
                        self.sessions.len(), connecting, Some(warning),
                        move |choice, remember, cx| {
                            if let Some(value) = choice.remembered(remember) {
                                workspace.update(cx, |ws, cx| ws.set_close_window_preference(value, cx))
                                    .map_err(|_| "窗口已关闭".to_string())??;
                            }
                            if let Some(scope) = scope.take() {
                                cx.defer(move |cx| scope.confirm(handle, choice, cx));
                            }
                            Ok(())
                        },
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
            self.set_window_hidden(true, window, cx);
            self.sync_menus(cx);
            native.orderOut(None);
            self.sync_hidden_activation_policy(cx);
            log::info!("main window hidden");
        }
        #[cfg(not(target_os = "macos"))]
        window.minimize_window();
    }

    pub(super) fn close_empty_window_if_needed(&self, window: &mut Window, cx: &App) -> bool {
        let close = window_close(Hub::windows(cx).len(), self.sessions.len(), false, CloseWindowWithSessions::Ask) == WindowClose::Remove;
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
        self.set_window_hidden(false, window, cx);
        self.sync_menus(cx);
        self.sync_sidebar(cx);
        self.sync_ai_context(cx);
        window.refresh();
        log::info!("main window shown");
    }

    fn set_window_hidden(&mut self, hidden: bool, window: &Window, cx: &mut Context<Self>) {
        self.hidden = hidden;
        // A hidden window draws no frames: its terminals are told here that they lost the focus.
        self.sync_terminal_focus(window, cx);
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

    pub(crate) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_window(window, cx);
        let impact = self.quit_impact(cx);
        if impact.needs_confirmation() && self.hub.preferences.borrow().get(Preference::ConfirmQuit) {
            self.confirm_quit(impact.message(), Some(impact), window, cx);
        } else {
            Hub::quit(cx);
        }
    }

    pub(super) fn confirm_quit(
        &mut self,
        message: String,
        review: Option<CloseImpact>,
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
                message,
                review,
                cx.weak_entity(),
                window,
                cx,
            ));
        }
    }
}

#[cfg(debug_assertions)]
pub(crate) fn debug_hooks(cx: &mut App) {
    super::tab_drag::debug::install(cx);
    super::zoom::debug_hook(cx);
    type Hook = (&'static str, fn(&mut App));
    let hooks: &[Hook] = &[
        ("SHELLMATE_DEBUG_DUPLICATE_AFTER", |cx| Hub::update(cx, Workspace::duplicate_active)),
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
        ("SHELLMATE_DEBUG_NOTIFY_AFTER", super::attention::debug_notify),
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
    fn nonfinal_window_policy_never_changes_last_or_empty_window_behavior() {
        for (policy, expected) in [(CloseWindowWithSessions::Ask, WindowClose::Confirm), (CloseWindowWithSessions::Move, WindowClose::Move), (CloseWindowWithSessions::Disconnect, WindowClose::Disconnect)] {
            assert_eq!(window_close(2, 3, true, policy), expected);
            assert_eq!(window_close(1, 3, true, policy), WindowClose::Last);
            assert_eq!(window_close(2, 0, false, policy), WindowClose::Remove);
            assert_eq!(window_close(1, 0, false, policy), WindowClose::Last);
        }
    }

    #[test]
    fn nonfinal_windows_confirm_only_when_they_have_tabs() {
        assert_eq!(window_close(1, 3, true, CloseWindowWithSessions::Ask), WindowClose::Last);
        assert_eq!(window_close(2, 3, true, CloseWindowWithSessions::Ask), WindowClose::Confirm);
    }

    #[test]
    fn losing_the_last_tab_closes_only_a_nonfinal_window() {
        assert_eq!(window_close(2, 0, false, CloseWindowWithSessions::Ask), WindowClose::Remove);
        assert_eq!(window_close(1, 0, false, CloseWindowWithSessions::Ask), WindowClose::Last);
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
    fn idle_windows_disconnect_without_asking_but_explicit_move_is_preserved() {
        assert_eq!(window_close(2, 5, false, CloseWindowWithSessions::Ask), WindowClose::Disconnect);
        assert_eq!(window_close(2, 5, true, CloseWindowWithSessions::Ask), WindowClose::Confirm);
        assert_eq!(window_close(2, 5, false, CloseWindowWithSessions::Move), WindowClose::Move);
        assert_eq!(window_close(2, 5, true, CloseWindowWithSessions::Disconnect), WindowClose::Disconnect);
    }
}
