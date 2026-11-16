//! Workspace integration: stable session targets, persistence, and palette actions.
use super::*;
#[cfg(debug_assertions)]
mod demo;
#[cfg(debug_assertions)]
mod live;
use crate::productivity::{
    model::{self, Action, Choice, SessionItem},
    panel::{Drawer, Event, Manager, PaletteData, Picker},
};
use shellmate_store::{HistoryEntry, Snippet};

impl Workspace {
    pub(crate) fn refresh_snippets(&mut self, cx: &mut Context<Self>) {
        match self.store.borrow().snippets() {
            Ok(groups) => self.commands.update(cx, |d, cx| {
                d.groups = groups;
                cx.notify();
            }),
            Err(_) => log::warn!("could not read quick commands"),
        }
        self.refresh_suggestions(cx);
        cx.notify();
    }

    pub(super) fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette_query("", window, cx);
    }
    fn open_palette_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        let data = PaletteData {
            book: self.book.clone(),
            sessions: self
                .sessions
                .iter()
                .map(|s| SessionItem {
                    id: s.id,
                    title: s.title.to_string(),
                    detail: if s.ssh().is_some() { "SSH" } else { "本机终端" }.into(),
                    host: s.ssh().map(|s| s.host.id.clone()),
                })
                .collect(),
            active: self.active.and_then(|i| self.sessions.get(i)).map(|s| s.id),
            snippets: self.commands.read(cx).groups.clone(),
            sidebar_open: self.sidebar_open,
            split: self.split.other.is_some(),
        };
        let picker = cx.new(|cx| Picker::palette(data, query, window, cx));
        self.install_picker(picker, window, cx);
    }
    pub(super) fn open_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_history_scope(false, window, cx);
    }
    fn open_history_scope(&mut self, current: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.show_panel(Panel::Terminal, window, cx);
        let host = self
            .focused_session()
            .map(|s| s.ssh().map_or_else(|| "local".into(), |s| s.host.id.to_string()));
        let picker = cx.new(|cx| Picker::history(self.store.clone(), host, current, window, cx));
        self.install_picker(picker, window, cx);
    }
    fn install_picker(&mut self, picker: Entity<Picker>, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_subscription = Some(cx.subscribe_in(&picker, window, |this, _, event, window, cx| {
            match event {
                Event::Close => {
                    this.picker = None;
                    this.focus_active(window, cx);
                }
                Event::Choose(choice) => {
                    let choice = choice.clone();
                    this.picker = None;
                    this.focus_active(window, cx);
                    this.choose(choice, window, cx);
                }
                _ => {}
            }
            cx.notify();
        }));
        self.picker = Some(picker);
        cx.notify();
    }
    fn choose(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        match choice {
            Choice::Quick(host) => self.connect_temporary(host, window, cx),
            Choice::Host(id) => self.connect_host(&id, window, cx),
            Choice::Session(id) => {
                if let Some(ix) = self.sessions.iter().position(|s| s.id == id) {
                    self.activate(ix, window, cx);
                }
            }
            Choice::Command(command) => self.confirm_command(command, window, cx),
            Choice::Action(action) => match action {
                Action::Local => self.open_local(window, cx),
                Action::NewHost => self.new_host(window, cx),
                Action::Split => self.toggle_split(window, cx),
                Action::Duplicate => self.duplicate_active(window, cx),
                Action::Files => self.show_panel(Panel::Files, window, cx),
                Action::Monitor => self.show_panel(Panel::Monitor, window, cx),
                Action::Forward => self.show_panel(Panel::Forward, window, cx),
                Action::Theme => self.toggle_theme(&ToggleTheme, window, cx),
                Action::Sidebar => self.toggle_sidebar(&ToggleSidebar, window, cx),
                Action::Import => self.open_import(window, cx),
                Action::Settings => self.open_settings(0,window,cx),
                Action::Records => self.open_records(None,false,window,cx),
                Action::History => self.open_history(window, cx),
            },
        }
    }
    pub(super) fn on_command_event(
        &mut self,
        _: &Entity<Drawer>,
        event: &Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            Event::Broadcast(enabled) => self.set_broadcast(*enabled, window, cx),
            Event::CheckSession(id) => {
                self.broadcast.toggle(*id, &self.session_rows(cx));
                self.sync_productivity(cx);
                cx.notify();
            }
            Event::Send(command) => self.confirm_command(command.clone(), window, cx),
            Event::Manage => Manager::open(
                self.commands.read(cx).groups.clone(),
                self.store.clone(),
                cx.entity().downgrade(),
                window,
                cx,
            ),
            Event::Samples => {
                let result = self.store.borrow_mut().save_snippets(&model::samples());
                match result {
                    Ok(()) => self.refresh_snippets(cx),
                    Err(_) => window.push_notification(Notification::error("无法保存示例命令"), cx),
                }
            }
            _ => {}
        }
    }
    fn confirm_command(&mut self, command: Snippet, window: &mut Window, cx: &mut Context<Self>) {
        let ids = if self.broadcast.enabled {
            self.broadcast.targets(&self.session_rows(cx))
        } else {
            self.focused_session().map(|s| s.id).into_iter().collect()
        };
        let request = self.send_request(ids, self.broadcast.enabled, false);
        self.open_confirmation(command, request, window, cx);
    }
    pub(crate) fn command_available(&self, id: Option<usize>, cx: &App) -> bool {
        self.sessions.iter().find(|s| Some(s.id) == id).is_some_and(|s| {
            s.terminal.read(cx).is_attached()
                && !s.terminal.read(cx).is_exited()
                && s.ssh().is_none_or(|s| s.status == ssh::SshStatus::Connected)
        })
    }
    pub(crate) fn send_reviewed(
        &mut self,
        request: &crate::productivity::multi::SendRequest,
        command: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let checked = request.targets.iter().map(|t| t.id).collect();
        let ids = crate::productivity::multi::connected_targets(&checked, &self.session_rows(cx));
        if ids.is_empty() {
            return Err("没有已连接的目标会话，无法发送".into());
        }
        // Validate every currently connected target before writing to the first one.
        for id in &ids {
            if let Some(session) = self.sessions.iter().find(|s| s.id == *id) {
                session
                    .terminal
                    .read(cx)
                    .validate_command(command)
                    .map_err(|error| format!("{}：{error}", session.title))?;
            }
        }
        let mut failed = Vec::new();
        let mut sent = 0;
        let results = crate::productivity::multi::fan_out(&ids, |id| {
            let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
                return Err("会话已关闭");
            };
            #[cfg(debug_assertions)]
            if self.live_review {
                log::info!("p6live send → {}", session.title);
            }
            session.terminal.update(cx, |t, _| t.send_command(command))
        });
        for (id, result) in results {
            if result.is_ok() {
                sent += 1;
            } else if let Some(session) = self.sessions.iter().find(|s| s.id == id) {
                failed.push(session.title.to_string());
            }
        }
        // Do not offer to retry a partial fan-out: a failed return can follow a successful paste.
        if !failed.is_empty() {
            window.push_notification(
                Notification::error(format!("发送失败：{}；请检查目标会话后再操作", failed.join("、"))),
                cx,
            );
        }
        if sent > 0 && request.clear_input {
            self.broadcast_bar.update(cx, |b, cx| b.clear(window, cx));
        }
        if !request.broadcast
            && let Some(target) = request.targets.first()
        {
            if self.split.other == Some(target.id) {
                self.split.focus = crate::productivity::multi::Pane::Other;
            } else if self.main_id() != Some(target.id)
                && let Some(ix) = self.sessions.iter().position(|s| s.id == target.id)
            {
                self.activate(ix, window, cx);
            }
        }
        self.show_panel(Panel::Terminal, window, cx);
        Ok(())
    }
    pub(super) fn save_command_history(
        &mut self,
        id: usize,
        command: &str,
        cwd: Option<String>,
        time: i64,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
            return;
        };
        let (host, label) = session.ssh().map_or_else(
            || ("local".into(), "本机".into()),
            |s| (s.host.id.to_string(), s.host.name.clone()),
        );
        let entry = HistoryEntry {
            id: 0,
            host,
            label,
            command: command.to_string(),
            cwd,
            time,
        };
        if self.store.borrow_mut().add_history(&entry).is_err() {
            log::warn!("could not save command history");
        }
        if self.broadcast.enabled {
            self.refresh_suggestions(cx);
        }
    }
}
