//! The single workspace window outlives its AppKit visibility.
use super::{SessionKind, Workspace, ssh::SshStatus};
use crate::dialogs::quit::QuitDialog;
use gpui::{AnyWindowHandle, App, Context, Entity, Global, WeakEntity, Window};
use shellmate_store::{Preference, Preferences};

#[derive(Clone)]
pub(crate) struct MainWindow {
    handle: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
}

impl Global for MainWindow {}

impl MainWindow {
    pub(crate) fn install(handle: AnyWindowHandle, workspace: &Entity<Workspace>, cx: &mut App) {
        let workspace = workspace.downgrade();
        cx.set_global(Self {
            handle,
            workspace: workspace.clone(),
        });
        handle
            .update(cx, |_, window, cx| {
                window.on_window_should_close(cx, move |window, cx| {
                    workspace
                        .update(cx, |s, cx| s.close_window(window, cx))
                        .ok();
                    false
                });
            })
            .expect("newly opened workspace window");
    }

    pub(crate) fn update(
        cx: &mut App,
        run: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        if let Some(main) = cx.try_global::<Self>().cloned() {
            // An action can bubble here from inside an update of this very window.
            cx.defer(move |cx| {
                main.handle
                    .update(cx, |_, window, cx| {
                        main.workspace.update(cx, |s, cx| run(s, window, cx)).ok();
                    })
                    .ok();
            });
        }
    }

    pub(crate) fn with_restored_window(
        cx: &mut App,
        run: impl FnOnce(&mut Window, &mut App) + 'static,
    ) {
        if let Some(main) = cx.try_global::<Self>().cloned() {
            cx.defer(move |cx| {
                main.handle
                    .update(cx, |_, w, cx| {
                        main.workspace.update(cx, |s, cx| s.show_window(w, cx)).ok();
                        run(w, cx);
                    })
                    .ok();
            });
        }
    }

    pub(crate) fn reopen(cx: &mut App) {
        if let Some(main) = cx.try_global::<Self>().cloned() {
            main.handle
                .update(cx, |_, w, cx| {
                    main.workspace.update(cx, |s, cx| s.show_window(w, cx)).ok();
                })
                .ok();
        }
    }
}

#[cfg(target_os = "macos")]
fn native_window(window: &Window) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
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
    Hide,
    Quit,
}

fn close_behavior(prefs: &Preferences) -> Close {
    if prefs.get(Preference::QuitOnWindowClose) {
        Close::Quit
    } else {
        Close::Hide
    }
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct Activity {
    pub(super) sessions: usize,
    pub(super) transfers: usize,
    pub(super) forwards: usize,
}

impl Activity {
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
    pub(crate) fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if close_behavior(&self.preferences) == Close::Quit {
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
            log::info!("main window hidden");
        }
        #[cfg(not(target_os = "macos"))]
        window.minimize_window();
    }

    pub(crate) fn show_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.hidden {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            let Some(native) = native_window(window) else {
                return;
            };
            native.makeKeyAndOrderFront(None);
        }
        self.set_window_hidden(false, cx);
        cx.activate(true);
        self.sync_menus(cx);
        self.sync_sidebar(cx);
        self.sync_ai_context(cx);
        window.refresh();
        log::info!("main window shown");
    }

    fn set_window_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        self.hidden = hidden;
        // The component input caret has its own timer independent of AppKit's display link.
        gpui::base::Theme::global_mut(cx).cursor_blink = if hidden {
            gpui::base::input::CursorBlink::steady()
        } else {
            gpui::component::Theme::global(cx).cursor_blink
        };
        for session in &self.sessions {
            session
                .view
                .update(cx, |view, cx| view.set_display_paused(hidden, cx));
        }
        if let Some(records) = self.records_window.as_ref().and_then(WeakEntity::upgrade) {
            records.update(cx, |panel, cx| panel.set_window_visible(!hidden, cx));
        }
        self.sync_monitor_activity(cx);
    }

    fn quit_activity(&self, cx: &mut Context<Self>) -> Activity {
        let sessions = self
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
            .count();
        let transfers = self
            .groups
            .iter()
            .filter_map(|g| g.link.as_ref())
            .map(|link| link.queue().running_count())
            .sum();
        let forwards = self
            .groups
            .iter()
            .filter_map(|g| g.forward.as_ref())
            .map(|f| f.forwarder.listening_rules())
            .sum();
        Activity {
            sessions,
            transfers,
            forwards,
        }
    }

    pub(crate) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_window(window, cx);
        let activity = self.quit_activity(cx);
        if activity.should_confirm(self.preferences.get(Preference::ConfirmQuit)) {
            self.confirm_quit(activity, window, cx);
        } else {
            cx.quit();
        }
    }

    pub(super) fn confirm_quit(
        &mut self,
        activity: Activity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .quit_dialog
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .is_none()
        {
            self.quit_dialog = Some(QuitDialog::open(
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
    let hooks: [Hook; 3] = [
        ("SHELLMATE_DEBUG_HIDE_AFTER", |cx| {
            MainWindow::update(cx, Workspace::close_window)
        }),
        ("SHELLMATE_DEBUG_REOPEN_AFTER", MainWindow::reopen),
        ("SHELLMATE_DEBUG_QUIT_AFTER", |cx| {
            cx.dispatch_action(&super::Quit)
        }),
    ];
    for (key, run) in hooks {
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
    fn close_decision_uses_preference() {
        let mut prefs = Preferences::default();
        assert_eq!(close_behavior(&prefs), Close::Hide);
        prefs.set(Preference::QuitOnWindowClose, true);
        assert_eq!(close_behavior(&prefs), Close::Quit);
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
