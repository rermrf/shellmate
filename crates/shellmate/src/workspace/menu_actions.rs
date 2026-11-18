use super::*;
use crate::menus::*;
use crate::productivity::multi::{SessionRow, Split};

#[derive(Default)]
struct Available {
    session: bool,
    remote_panels: bool,
    duplicate: bool,
    split: bool,
    recording: bool,
    cycle: bool,
    vault_lock: bool,
}

type Handler = fn(&mut Workspace, &mut Window, &mut Context<Workspace>);

impl Available {
    fn for_sessions(rows: &[SessionRow], active: Option<(usize, Panel)>, focused: Option<usize>, split: &Split, recording: bool) -> Self {
        let main = active.and_then(|(id, _)| rows.iter().find(|row| row.id == id));
        let protocol = main.filter(|row| !row.local).map(|row| row.protocol);
        Self {
            session: main.is_some(),
            remote_panels: main.is_some() && Panel::Files.supported(protocol),
            duplicate: main.is_some() && Session::duplicable(protocol),
            split: active.is_some_and(|(id, panel)| panel == Panel::Terminal &&
                (split.other.is_some() || Split::candidate(id, rows, &[]).is_some())),
            recording: recording || rows.iter().any(|row| Some(row.id) == focused && row.connected()),
            cycle: rows.len() >= 2,
            vault_lock: false,
        }
    }

    fn listeners(&self, mut bind: impl FnMut(&'static dyn gpui::Action, Handler)) {
        bind(&Settings, |workspace, window, cx| workspace.open_settings(0, window, cx));
        if self.vault_lock { bind(&LockVault, Workspace::lock_vault); }
        bind(&NewLocalTerminal, Workspace::open_local);
        bind(&NewConnection, Workspace::new_host);
        if self.duplicate { bind(&DuplicateSession, Workspace::duplicate_active); }
        if self.split { bind(&ToggleSplit, Workspace::toggle_split); }
        if self.recording { bind(&ToggleRecording, Workspace::toggle_recording); }
        bind(&ToggleBroadcast, Workspace::toggle_broadcast);
        bind(&ImportHosts, Workspace::open_import);
        bind(&ImportBundle, Workspace::import_bundle);
        bind(&ExportBundle, Workspace::export_bundle);
        if self.session {
            bind(&CloseTab, Workspace::close_tab);
            bind(&ShowTerminal, |workspace, window, cx| workspace.show_panel(Panel::Terminal, window, cx));
        }
        bind(&ToggleSidebar, |workspace, window, cx| workspace.toggle_sidebar(&ToggleSidebar, window, cx));
        if self.remote_panels {
            bind(&ShowFiles, |workspace, window, cx| workspace.show_panel(Panel::Files, window, cx));
            bind(&ShowForward, |workspace, window, cx| workspace.show_panel(Panel::Forward, window, cx));
            bind(&ShowMonitor, |workspace, window, cx| workspace.show_panel(Panel::Monitor, window, cx));
        }
        bind(&ToggleTheme, |workspace, window, cx| workspace.toggle_theme(&ToggleTheme, window, cx));
        bind(&OpenPalette, |workspace, window, cx| workspace.open_palette(&OpenPalette, window, cx));
        bind(&History, Workspace::open_history);
        bind(&ToggleCommands, Workspace::toggle_commands);
        bind(&ToggleAi, Workspace::toggle_ai);
        bind(&Records, |workspace, window, cx| workspace.open_records(None, false, window, cx));
        if self.cycle {
            bind(&PreviousTab, Workspace::previous_tab);
            bind(&NextTab, Workspace::next_tab);
        }
    }
}

impl Workspace {
    /// Only the master-password vault can be locked; the system keychain unlocks with the login session.
    pub(super) fn vault_lock_available(&self) -> bool {
        self.secrets.backend() == shellmate_store::Backend::Master
    }

    pub(super) fn lock_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.secrets.lock();
        window.push_notification(Notification::info("凭据库已锁定"), cx);
    }

    pub(super) fn toggle_ai(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_open { self.close_ai(cx); } else { self.open_ai(window, cx); }
    }

    pub(super) fn toggle_commands(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.commands_open = !self.commands_open;
        self.close_ai(cx);
    }

    pub(super) fn toggle_broadcast(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_ai(cx);
        self.set_broadcast(!self.broadcast.enabled, window, cx);
    }

    pub(super) fn sync_menus(&mut self, cx: &mut Context<Self>) {
        let state = MenuState {
            sidebar_open: self.sidebar_open,
            panel: self.active.and_then(|index| self.sessions.get(index)).map(|session| session.panel),
            split: self.split.other.is_some(),
            recording: self.focused_session().is_some_and(|session| self.running_recordings.contains_key(&session.id)),
            broadcast: self.broadcast.enabled,
            dark: cx.is_dark(),
            commands_open: self.commands_open,
            ai_open: self.ai_open,
            vault_lock: self.vault_lock_available(),
        };
        if self.last_menu_state != Some(state) {
            self.last_menu_state = Some(state);
            cx.defer(move |cx| cx.set_menus(menus(&state)));
        }
    }

    pub(super) fn menu_listeners<E: InteractiveElement>(&self, mut root: E, cx: &Context<Self>) -> E {
        let session = self.active.and_then(|index| self.sessions.get(index));
        let focused_id = self.focused_session().map(|session| session.id);
        let mut available = Available::for_sessions(&self.session_rows(cx), session.map(|s| (s.id, s.panel)), focused_id,
            &self.split, focused_id.is_some_and(|id| self.running_recordings.contains_key(&id)));
        available.vault_lock = self.vault_lock_available();
        available.listeners(|action, handler| {
            root.interactivity().on_boxed_action(action, cx.listener(move |workspace, _, window, cx| handler(workspace, window, cx)));
        });
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Action as _;
    use shellmate_core::Protocol;
    use crate::productivity::multi::Status;

    #[test]
    fn workspace_handlers_for_main_local_empty_and_serial() {
        let row = |id, protocol: Option<Protocol>, status| SessionRow {
            id, local: protocol.is_none(), protocol: protocol.unwrap_or_default(), status, title: String::new(),
        };
        let ssh = row(1, Some(Protocol::Ssh), Status::Connected);
        let local = row(5, None, Status::Connected);
        let serial = row(3, Some(Protocol::Serial), Status::Offline);
        for (scene, rows, active, flags) in [
            ("proto:main", vec![ssh.clone(), local.clone()], Some(1), [true,true,true,true,true,true]),
            ("proto:local", vec![ssh, local.clone()], Some(5), [true,false,true,true,true,true]),
            ("menu-empty", vec![], None, [false,false,false,false,false,false]),
            ("one-local", vec![local], Some(5), [true,false,true,false,true,false]),
            ("serial-offline", vec![serial], Some(3), [true,false,false,false,false,false]),
        ] {
            let available = Available::for_sessions(&rows, active.map(|id| (id, Panel::Terminal)), active, &Split::default(), false);
            let mut registered = std::collections::HashSet::new();
            available.listeners(|action, _| { assert!(registered.insert(action.name())); });
            assert!(!registered.contains(LockVault.name()), "{scene}: the keychain cannot be locked");
            for (commands, flag) in [
                (vec![CloseTab.name(), ShowTerminal.name()], flags[0]),
                (vec![ShowFiles.name(), ShowForward.name(), ShowMonitor.name()], flags[1]),
                (vec![DuplicateSession.name()], flags[2]), (vec![ToggleSplit.name()], flags[3]),
                (vec![ToggleRecording.name()], flags[4]), (vec![NextTab.name(), PreviousTab.name()], flags[5]),
            ] {
                for command in commands { assert_eq!(registered.contains(command), flag, "{scene} {command}"); }
            }
            if scene == "proto:main" {
                for item in menus(&MenuState::default()).into_iter().flat_map(|menu| menu.items) {
                    if let gpui::MenuItem::Action { action, .. } = item {
                        let name = action.name();
                        let outside_workspace = [About.name(), HideApp.name(), HideOthers.name(), ShowAll.name(), Quit.name(),
                            EditUndo.name(), EditRedo.name(), EditCut.name(), EditCopy.name(), EditPaste.name(), EditSelectAll.name(),
                            Fullscreen.name(), Minimize.name(), Zoom.name(), ToggleSearch.name(),
                            shellmate_term::view::SearchUp.name(), shellmate_term::view::SearchDown.name()];
                        assert!(outside_workspace.contains(&name) || registered.contains(name), "no handler for {name}");
                    }
                }
            }
        }
    }
}
