//! Application-owned services and routing to the current owner of a tab.
use super::*;
use gpui::{AnyWindowHandle, Global, WeakEntity};
use std::cell::{Cell, RefCell};
use std::time::Duration;

#[derive(Default)]
pub(super) struct Ids(Cell<usize>);

impl Ids {
    pub(super) fn allocate(&self) -> usize {
        let id = self.0.get().checked_add(1).expect("id space exhausted");
        self.0.set(id);
        id
    }
}

pub(super) struct Services {
    pub store: shellmate_store::SharedStore,
    pub secrets: Arc<shellmate_store::CredentialStore>,
    pub book: RefCell<Rc<HostBook>>,
    pub preferences: RefCell<shellmate_store::Preferences>,
    pub data_root: RefCell<PathBuf>,
    pub log_root: RefCell<PathBuf>,
    pub library_is_temporary: bool,
    pub user_host: RefCell<SharedString>,
    pub pending_forms: RefCell<HashMap<HostId, (connect::Endpoint, connect::FormCredential)>>,
    pub temporary_hosts: RefCell<HashMap<HostId, shellmate_core::Host>>,
    pub groups: RefCell<Vec<group::ConnGroup>>,
    pub local_monitor: RefCell<Option<group::GroupMonitor>>,
    pub forward_grants: shellmate_forward::GrantMemory,
    pub session_ids: Ids,
    pub group_ids: Ids,
    pub recent_sessions: RefCell<Vec<usize>>,
    pub running_recordings: RefCell<HashMap<usize, archive::RunningRecording>>,
    pub records_window: RefCell<Option<WeakEntity<crate::records::panel::RecordsWindow>>>,
    pub quit_dialog: RefCell<Option<WeakEntity<crate::dialogs::quit::QuitDialog>>>,
    #[cfg(target_os = "macos")]
    pub status_bar: RefCell<status_item::StatusBar>,
}

#[derive(Clone)]
pub(crate) struct Entry {
    pub handle: AnyWindowHandle,
    pub workspace: WeakEntity<Workspace>,
}

impl Entry {
    pub(super) fn update<R>(
        &self,
        cx: &mut App,
        run: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> R,
    ) -> Option<R> {
        let workspace = self.workspace.upgrade()?;
        self.handle
            .update(cx, |_, window, cx| {
                workspace.update(cx, |s, cx| run(s, window, cx))
            })
            .ok()
    }
}

#[derive(Clone, Copy)]
pub(super) enum Route {
    All,
    Group(usize),
    Session(usize),
    Local,
}

impl Route {
    fn matches(self, workspace: &Workspace) -> bool {
        matches!(self, Self::All)
            || workspace.sessions.iter().any(|s| {
                self.includes(
                    s.id,
                    s.ssh().map(|ssh| ssh.group),
                    matches!(s.kind, SessionKind::Local),
                )
            })
    }
    fn includes(self, session: usize, group: Option<usize>, local: bool) -> bool {
        match self {
            Self::All => true,
            Self::Group(id) => group == Some(id),
            Self::Session(id) => session == id,
            Self::Local => local,
        }
    }

    pub(super) fn target(self, cx: &App) -> Option<Entry> {
        Hub::windows(cx)
            .iter()
            .find(|e| {
                e.workspace
                    .upgrade()
                    .is_some_and(|w| self.matches(w.read(cx)))
            })
            .cloned()
    }

    /// Resolve after an await, when the tab or its sibling may be in a different window.
    pub(super) fn deliver<R>(
        self,
        cx: &mut gpui::AsyncApp,
        run: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> R,
    ) -> Option<R> {
        cx.update(|cx| self.target(cx)?.update(cx, run))
    }
}

pub(crate) struct Hub {
    pub(super) services: Rc<Services>,
    windows: Vec<Entry>,
    _timer: gpui::Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Global for Hub {}

fn make_recent<T>(entries: &mut [T], matches: impl Fn(&T) -> bool) {
    if let Some(index) = entries.iter().position(matches) {
        entries[..=index].rotate_right(1);
    }
}

impl Hub {
    pub fn init(store: HostStore, temporary: bool, cx: &mut App) {
        let preferences = store.preferences().unwrap_or_default();
        let store = shellmate_store::SharedStore::new(store);
        let secrets = crate::security::coordinator(store.clone());
        crate::updater::init(store.clone(), cx);
        let activity = secrets.clone();
        let keys = cx.intercept_keystrokes(move |_, _, _| activity.activity());
        let closed = cx.on_window_closed(|cx, id| {
            let windows = &mut cx.global_mut::<Self>().windows;
            if let Some(ix) = windows.iter().position(|e| e.handle.window_id() == id) {
                windows.remove(ix);
                log::info!("window closed {}", id.as_u64());
                Self::update(cx, |s, _, cx| {
                    s.last_menu_state = None;
                    s.sync_menus(cx);
                    cx.notify();
                });
            }
        });
        let timer = cx.spawn(async |cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                cx.update(|cx| {
                    let hub = cx.global::<Self>();
                    let locked = hub.services.secrets.tick();
                    for (ix, entry) in hub.windows.clone().into_iter().enumerate() {
                        entry.update(cx, |s, w, cx| {
                            if ix == 0 && locked {
                                w.push_notification(Notification::info("凭据库已锁定"), cx);
                            }
                            s.tick_archives(w, cx);
                        });
                    }
                });
            }
        });
        #[cfg(target_os = "macos")]
        let status_bar = status_item::StatusBar::new(cx).into();
        cx.set_global(Self {
            services: Rc::new(Services {
                store,
                secrets,
                preferences: preferences.into(),
                book: Default::default(),
                library_is_temporary: temporary,
                user_host: RefCell::new(user_at_host().into()),
                data_root: shellmate_term::archive::data_root().into(),
                log_root: shellmate_term::archive::log_root().into(),
                groups: Default::default(),
                pending_forms: Default::default(),
                temporary_hosts: Default::default(),
                local_monitor: Default::default(),
                forward_grants: shellmate_forward::GrantMemory::new(),
                session_ids: Default::default(),
                group_ids: Default::default(),
                recent_sessions: Default::default(),
                running_recordings: Default::default(),
                records_window: Default::default(),
                quit_dialog: Default::default(),
                #[cfg(target_os = "macos")]
                status_bar,
            }),
            windows: Vec::new(),
            _timer: timer,
            _subscriptions: vec![closed, keys],
        });
    }

    pub(crate) fn windows(cx: &App) -> &[Entry] {
        cx.try_global::<Self>()
            .map_or(&[], |hub| hub.windows.as_slice())
    }

    pub fn install(handle: AnyWindowHandle, workspace: &Entity<Workspace>, cx: &mut App) {
        let workspace = workspace.downgrade();
        cx.global_mut::<Self>().windows.insert(
            0,
            Entry {
                handle,
                workspace: workspace.clone(),
            },
        );
        log::info!("window opened {}", handle.window_id().as_u64());
        handle
            .update(cx, |_, w, cx| {
                w.on_window_should_close(cx, move |w, cx| {
                    workspace.update(cx, |s, cx| s.close_window(w, cx)).ok();
                    false
                })
            })
            .expect("newly opened workspace window");
    }

    pub(super) fn activated(handle: AnyWindowHandle, cx: &mut App) {
        make_recent(&mut cx.global_mut::<Self>().windows, |e| e.handle == handle);
    }

    /// Finalize each window's archives before a confirmed quit or updater restart.
    pub(crate) fn quit(cx: &mut App) {
        cx.defer(|cx| {
            for entry in Self::windows(cx).to_vec() {
                if let Some(workspace) = entry.workspace.upgrade() {
                    workspace.update(cx, Workspace::finish_archives);
                }
            }
            cx.quit();
        });
    }

    pub(crate) fn update(
        cx: &mut App,
        run: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        cx.defer(move |cx| {
            if let Some(entry) = Self::windows(cx).first().cloned() {
                entry.update(cx, run);
            }
        });
    }

    pub(super) fn with_restored_workspace(
        cx: &mut App,
        run: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        Self::update(cx, move |s, w, cx| {
            s.show_window(w, cx);
            run(s, w, cx);
        });
    }
    pub(crate) fn with_restored_window(
        cx: &mut App,
        run: impl FnOnce(&mut Window, &mut App) + 'static,
    ) {
        Self::with_restored_workspace(cx, move |_, w, cx| run(w, cx));
    }
    pub(crate) fn reopen(cx: &mut App) {
        Self::update(cx, Workspace::show_window);
    }
}

impl Workspace {
    /// Apply a projection to each owning workspace once. The callback only updates local views.
    pub(super) fn project(
        &mut self,
        route: Route,
        cx: &mut Context<Self>,
        mut apply: impl FnMut(&mut Workspace, &mut Context<Workspace>),
    ) {
        for entry in self.peers(route, cx) {
            if let Some(w) = entry.workspace.upgrade() {
                w.update(cx, &mut apply);
            }
        }
        apply(self, cx);
    }

    /// The caller already holds its own entity. Only inspect peers here.
    pub(super) fn peers(&self, route: Route, cx: &App) -> Vec<Entry> {
        Hub::windows(cx)
            .iter()
            .filter(|e| {
                e.handle != self.handle
                    && e.workspace
                        .upgrade()
                        .is_some_and(|w| route.matches(w.read(cx)))
            })
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocators_are_shared_across_window_handles() {
        let sessions = Rc::new(Ids::default());
        let groups = Rc::new(Ids::default());
        let first = (sessions.clone(), groups.clone());
        let second = (sessions, groups);
        assert_eq!(
            (first.0.allocate(), second.0.allocate(), first.0.allocate()),
            (1, 2, 3)
        );
        assert_eq!((second.1.allocate(), first.1.allocate()), (1, 2));
    }

    #[test]
    fn activation_promotes_once_and_preserves_the_order_of_peers() {
        let mut entries = [3, 2, 1];
        for key in [1, 3, 3, 4] {
            make_recent(&mut entries, |id| *id == key);
        }
        assert_eq!(entries, [3, 1, 2]);
    }

    #[test]
    fn group_targets_follow_membership_in_activation_order() {
        let mut windows = [
            vec![(1, Some(4)), (2, Some(4))],
            vec![(3, Some(9))],
            vec![(4, Some(4))],
        ];
        let targets = |windows: &[Vec<(usize, Option<usize>)>]| {
            windows
                .iter()
                .enumerate()
                .filter(|(_, tabs)| {
                    tabs.iter()
                        .any(|(id, g)| Route::Group(4).includes(*id, *g, false))
                })
                .map(|(ix, _)| ix)
                .collect::<Vec<_>>()
        };
        assert_eq!(targets(&windows), [0, 2]);
        let tab = windows[0].remove(0);
        windows[1].push(tab);
        assert_eq!(targets(&windows), [0, 1, 2]);
        windows[0].clear();
        assert_eq!(targets(&windows), [1, 2]);
    }
}
