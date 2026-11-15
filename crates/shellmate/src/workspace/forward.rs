//! The 端口转发 panel inside the workspace: one panel per SSH tab, all of a host's panels on the one
//! set of rules.
//!
//! Every connection ([`ConnGroup`]) has a [`Forwarder`]; the rules of a saved host live in the host
//! library and run on **one** connection to it, the *owner*: the one with the lowest number among the
//! connections to that host (a copied tab on a changed host, or a tab that had to connect on its
//! own, can make a second one). The panels of the other connections show the same rules and say
//! where they run. When the owner goes away the next connection takes the rules over. The rules of a
//! host that is not saved (deleted while it is open, or a temporary library) only exist in memory.

use gpui::component::WindowExt as _;
use gpui::component::notification::Notification;
use gpui::{AppContext as _, Context, Entity, Subscription, Window};
use shellmate_core::{Host, HostId};
use shellmate_forward::{ForwardEvent, ForwardRule, Forwarder, Options, RuleId};
use shellmate_ssh::Connection;

use super::Workspace;
use super::group::{self, GroupForward};
use crate::dialogs::confirm::ConfirmDialog;
use crate::forward::model::{self, ForwardView, RetryAction, RuleRuntime, TEMPORARY_NOTE};
use crate::forward::panel::{ForwardPanel, ForwardPanelEvent};
use crate::runtime;

/// The forwarding panel of a tab and what keeps it wired to the workspace.
pub struct ForwardTab {
    pub panel: Entity<ForwardPanel>,
    _subscription: Subscription,
}

impl Workspace {
    // ---- the engine of a connection ------------------------------------------------------------

    /// Gives a new connection its forwarder and starts following what it says.
    pub(super) fn attach_forwarder(&mut self, group_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(runtime) = runtime::get(cx) else { return };
        let (events, changes) = async_channel::unbounded();
        let options = Options { grants: self.forward_grants.clone(), ..Options::default() };
        let forwarder = Forwarder::with_options(runtime.handle(), events, options);
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = changes.recv().await {
                if let ForwardEvent::State { id, state } = &event {
                    log::debug!("端口转发 规则 {id:?} → {state:?}");
                }
                if this.update_in(cx, |this, _, cx| this.sync_forward_group(group_id, cx)).is_err() {
                    break;
                }
            }
        });
        if let Some(group) = self.group_mut(group_id) {
            group.forward =
                Some(GroupForward { forwarder, rules: Vec::new(), loaded: false, started: false, _task: Some(task) });
        }
    }

    /// The connection that carries the rules of `host`: the one with the lowest number.
    pub(super) fn forward_owner(&self, host: &HostId) -> Option<usize> {
        group::forward_owner(&self.groups, host)
    }

    /// Whether the rules of `host` are kept in the host library (the host is saved there).
    fn forward_persisted(&self, host: &HostId) -> bool {
        !self.library_is_temporary && matches!(self.store.borrow().host(host), Ok(Some(_)))
    }

    /// Reads the rules of the owner from the library (once; later changes go through [`Workspace`]
    /// and keep the copy in the group in step).
    fn ensure_rules(&mut self, owner: usize) {
        let Some(group) = self.group(owner) else { return };
        let host = group.key.host().clone();
        if group.forward.as_ref().is_none_or(|forward| forward.loaded) {
            return;
        }
        let persisted = self.forward_persisted(&host);
        let rules = if persisted {
            match self.store.borrow().forward_rules(&host) {
                Ok(rules) => rules,
                Err(err) => {
                    log::error!("could not read the forwarding rules: {err}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        if let Some(forward) = self.group_mut(owner).and_then(|group| group.forward.as_mut()) {
            forward.rules = rules;
            forward.loaded = true;
        }
    }

    /// The connection came up (the first time, or after a reconnect). The engine takes it; on the
    /// first time the enabled rules start.
    pub(super) fn forward_connected(&mut self, group_id: usize, connection: &Connection, cx: &mut Context<Self>) {
        if let Some(forward) = self.group(group_id).and_then(|group| group.forward.as_ref()) {
            forward.forwarder.connection_restored(connection);
        }
        self.forward_start_enabled(group_id, cx);
        self.sync_forward_group(group_id, cx);
    }

    /// The connection was lost: local listeners stay, remote ones are asked for again after the reconnect.
    pub(super) fn forward_lost(&mut self, group_id: usize, cx: &mut Context<Self>) {
        if let Some(forward) = self.group(group_id).and_then(|group| group.forward.as_ref()) {
            forward.forwarder.connection_lost();
        }
        self.sync_forward_group(group_id, cx);
    }

    /// Starts the enabled rules of the host on the connection, if it carries them, is up and has not
    /// started them yet.
    fn forward_start_enabled(&mut self, group_id: usize, _cx: &mut Context<Self>) {
        let Some(host) = self.group(group_id).map(|group| group.key.host().clone()) else { return };
        if self.forward_owner(&host) != Some(group_id) {
            return;
        }
        self.ensure_rules(group_id);
        let Some(group) = self.group_mut(group_id) else { return };
        let up = group.live_connection().is_some();
        let Some(forward) = group.forward.as_mut().filter(|forward| up && !forward.started) else { return };
        forward.started = true;
        for rule in forward.rules.iter().filter(|rule| rule.enabled) {
            forward.forwarder.start(rule.clone());
        }
    }

    /// A connection to `host` is gone (its last tab closed) and carried these `rules`: the next
    /// connection to the host takes them over.
    pub(super) fn forward_handed_over(&mut self, host: &HostId, rules: Vec<ForwardRule>, cx: &mut Context<Self>) {
        if let Some(owner) = self.forward_owner(host) {
            if let Some(forward) = self.group_mut(owner).and_then(|group| group.forward.as_mut()) {
                forward.rules = rules;
                forward.loaded = true;
            }
            self.forward_start_enabled(owner, cx);
        }
        self.sync_forward(host, cx);
    }

    // ---- what the panels show ------------------------------------------------------------------

    /// Tells every forwarding panel of a connection's host where things stand.
    pub(super) fn sync_forward_group(&mut self, group_id: usize, cx: &mut Context<Self>) {
        if let Some(host) = self.group(group_id).map(|group| group.key.host().clone()) {
            self.sync_forward(&host, cx);
        }
    }

    /// Hands the current view to every forwarding panel of the tabs on `host`.
    pub(super) fn sync_forward(&mut self, host: &HostId, cx: &mut Context<Self>) {
        let tabs: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|s| s.ssh().filter(|ssh| &ssh.host.id == host).zip(s.forward.as_ref()))
            .map(|(ssh, tab)| (ssh.group, ssh.host.clone(), tab.panel.clone()))
            .collect();
        if let Some(owner) = self.forward_owner(host) {
            self.ensure_rules(owner);
        }
        for (group_id, host, panel) in tabs {
            let view = self.forward_view(group_id, &host);
            panel.update(cx, |panel, cx| panel.set_view(view, cx));
        }
    }

    /// The view of the tab on connection `group_id` to `host`: the rules of the host as the
    /// connection that carries them runs them.
    fn forward_view(&self, group_id: usize, host: &Host) -> ForwardView {
        let owner_id = self.forward_owner(&host.id).unwrap_or(group_id);
        let mut notes = Vec::new();
        if !self.forward_persisted(&host.id) {
            notes.push(TEMPORARY_NOTE.to_string());
        }
        if owner_id != group_id {
            let tab = self
                .sessions
                .iter()
                .find(|s| s.ssh().is_some_and(|ssh| ssh.group == owner_id))
                .map_or_else(|| "另一个标签".to_string(), |s| format!("「{}」", s.title));
            notes.push(format!("这些规则在到此主机的另一条连接（标签{tab}）上运行，这里显示的是同一份。"));
        }
        let Some(forward) = self.group(owner_id).and_then(|group| group.forward.as_ref()) else {
            return ForwardView::empty(&host.name);
        };
        let views = forward.forwarder.views();
        let runtime: Vec<RuleRuntime> = forward
            .rules
            .iter()
            .map(|rule| match views.iter().find(|view| view.rule.id == rule.id) {
                Some(view) => RuleRuntime::Started { state: view.state.clone(), stats: view.stats },
                None if rule.enabled => RuleRuntime::NotStarted,
                None => RuleRuntime::Disabled,
            })
            .collect();
        model::summary_view(&host.name, &forward.rules, &runtime, forward.forwarder.running_rules(), &notes)
    }

    /// Creates the forwarding panel of the tab at `ix` if it has none yet. `false` if it cannot be made.
    pub(super) fn ensure_forward_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.sessions[ix].forward.is_some() {
            return true;
        }
        let Some((session_id, host, group_id)) =
            self.sessions[ix].ssh().map(|ssh| (self.sessions[ix].id, ssh.host.clone(), ssh.group))
        else {
            return false;
        };
        if let Some(owner) = self.forward_owner(&host.id) {
            self.ensure_rules(owner);
        }
        let view = self.forward_view(group_id, &host);
        let panel = cx.new(|cx| ForwardPanel::new(view, cx));
        let subscription =
            cx.subscribe_in(&panel, window, move |this: &mut Self, _, event: &ForwardPanelEvent, window, cx| {
                this.on_forward_event(session_id, event, window, cx)
            });
        self.sessions[ix].forward = Some(ForwardTab { panel, _subscription: subscription });
        true
    }

    // ---- what the user does --------------------------------------------------------------------

    /// What the panel of tab `session_id` asked for.
    fn on_forward_event(
        &mut self,
        session_id: usize,
        event: &ForwardPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) =
            self.sessions.iter().find(|s| s.id == session_id).and_then(|s| s.ssh()).map(|ssh| ssh.host.id.clone())
        else {
            return;
        };
        match event {
            ForwardPanelEvent::Add(rule) => self.add_rule(&host, rule.clone(), window, cx),
            ForwardPanelEvent::Toggle(id, on) => self.toggle_rule(&host, id, *on, window, cx),
            ForwardPanelEvent::Retry(id, action) => self.retry_rule(&host, id, *action, window, cx),
            ForwardPanelEvent::Delete(id) => self.confirm_delete_rule(&host, id, window, cx),
        }
    }

    /// Runs `change` on the rules of `host` as the connection that carries them has them. The
    /// closure sees the group's data and the persistence flag; its error is shown as a notification.
    fn with_rules<R>(
        &mut self,
        host: &HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut GroupForward) -> Result<R, String>,
    ) -> Option<R> {
        let owner = self.forward_owner(host)?;
        self.ensure_rules(owner);
        let forward = self.group_mut(owner)?.forward.as_mut()?;
        match change(forward) {
            Ok(result) => Some(result),
            Err(message) => {
                window.push_notification(Notification::error(message).title("端口转发"), cx);
                None
            }
        }
    }

    /// Saves a change to the rules in the host library if the host is saved there.
    fn persist(
        &mut self,
        host: &HostId,
        save: impl FnOnce(&mut shellmate_store::HostStore) -> shellmate_store::Result<()>,
    ) -> Result<(), String> {
        if !self.forward_persisted(host) {
            return Ok(());
        }
        save(&mut self.store.borrow_mut()).map_err(|err| format!("无法保存端口转发规则：{err}"))
    }

    fn add_rule(&mut self, host: &HostId, rule: ForwardRule, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(message) = self.persist(host, |store| store.save_forward_rule(host, &rule)) {
            window.push_notification(Notification::error(message).title("端口转发"), cx);
            return;
        }
        self.with_rules(host, window, cx, |forward| {
            forward.rules.push(rule.clone());
            if forward.started {
                forward.forwarder.start(rule);
            }
            Ok(())
        });
        self.sync_forward(host, cx);
    }

    fn toggle_rule(&mut self, host: &HostId, id: &RuleId, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(message) = self.persist(host, |store| store.set_forward_rule_enabled(id, on)) {
            window.push_notification(Notification::error(message).title("端口转发"), cx);
            return;
        }
        self.with_rules(host, window, cx, |forward| {
            if let Some(rule) = forward.rules.iter_mut().find(|rule| rule.id == *id) {
                rule.enabled = on;
                if on {
                    if forward.started {
                        forward.forwarder.start(rule.clone());
                    }
                } else {
                    forward.forwarder.stop(id);
                }
            }
            Ok(())
        });
        self.sync_forward(host, cx);
    }

    /// A retry button: 换用 M 重试 (the rule listens on the other port from now on, saved, and starts) or
    /// 重试 / 立即重试 (the server is asked for the remote port again, now).
    fn retry_rule(
        &mut self,
        host: &HostId,
        id: &RuleId,
        action: RetryAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let port = match action {
            RetryAction::OtherPort(port) => port,
            RetryAction::Again => {
                self.with_rules(host, window, cx, |forward| {
                    let asked = forward.forwarder.retry(id);
                    log::debug!("端口转发 规则 {id:?} 重试：{}", if asked { "已再次向服务器请求" } else { "无需重试" });
                    Ok(())
                });
                return self.sync_forward(host, cx);
            }
        };
        let Some(moved) = self.with_rules(host, window, cx, |forward| {
            Ok(forward.rules.iter().find(|rule| rule.id == *id).map(|rule| model::with_listen_port(rule, port)))
        }) else {
            return;
        };
        let Some(moved) = moved else { return };
        if let Err(message) = self.persist(host, |store| store.save_forward_rule(host, &moved)) {
            window.push_notification(Notification::error(message).title("端口转发"), cx);
            return;
        }
        self.with_rules(host, window, cx, |forward| {
            if let Some(slot) = forward.rules.iter_mut().find(|rule| rule.id == moved.id) {
                *slot = moved.clone();
            }
            if forward.started && moved.enabled {
                forward.forwarder.start(moved);
            }
            Ok(())
        });
        self.sync_forward(host, cx);
    }

    /// Asks before deleting a rule (it says what it will cut).
    fn confirm_delete_rule(&mut self, host: &HostId, id: &RuleId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) =
            self.sessions.iter().find(|s| s.ssh().is_some_and(|ssh| &ssh.host.id == host)).and_then(|s| s.ssh())
        else {
            return;
        };
        let view = self.forward_view(tab.group, &tab.host);
        let Some(card) = view.cards.into_iter().find(|card| card.id == *id) else { return };
        let (title, text) = model::delete_confirmation(&card);
        let (workspace, host, id) = (cx.entity().downgrade(), host.clone(), id.clone());
        ConfirmDialog::open(
            title,
            text,
            "删除规则",
            true,
            move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| workspace.delete_rule(&host, &id, window, cx))
                    .map_err(|err| err.to_string())?
            },
            window,
            cx,
        );
    }

    fn delete_rule(
        &mut self,
        host: &HostId,
        id: &RuleId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.persist(host, |store| store.delete_forward_rule(id))?;
        let name = self
            .with_rules(host, window, cx, |forward| {
                forward.forwarder.stop(id);
                let name = forward.rules.iter().find(|rule| rule.id == *id).map(|rule| rule.name.clone());
                forward.rules.retain(|rule| rule.id != *id);
                Ok(name)
            })
            .flatten();
        self.sync_forward(host, cx);
        if let Some(name) = name {
            window.push_notification(Notification::success(format!("已删除规则 {name}")), cx);
        }
        Ok(())
    }

    // ---- closing the last tab -----------------------------------------------------------------

    /// Closes the tab at `ix`. The last tab of a connection that has forwards running asks first:
    /// the forwards stop with it.
    pub(super) fn close(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.close_question(ix) {
            Some((session_id, title, text)) => {
                let workspace = cx.entity().downgrade();
                ConfirmDialog::open(
                    title,
                    text,
                    "关闭",
                    true,
                    move |window, cx| {
                        workspace.update(cx, |workspace, cx| workspace.close_session(session_id, window, cx)).ok();
                        Ok(())
                    },
                    window,
                    cx,
                );
            }
            None => self.close_now(ix, window, cx),
        }
    }

    /// Closes the tab with this id if it is still there (the answer to the question above).
    fn close_session(&mut self, session_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.sessions.iter().position(|s| s.id == session_id) {
            self.close_now(ix, window, cx);
        }
    }

    /// What to ask before closing the tab at `ix`, if anything: the tab's id, the title and the text.
    fn close_question(&self, ix: usize) -> Option<(usize, String, String)> {
        let session = self.sessions.get(ix)?;
        let ssh = session.ssh()?;
        let group = self.group(ssh.group)?;
        let others_on_connection =
            self.sessions.iter().filter(|s| s.ssh().is_some_and(|other| other.group == ssh.group)).count() - 1;
        if others_on_connection > 0 || self.forward_owner(&ssh.host.id) != Some(group.id) {
            return None;
        }
        // Another connection to the host takes the rules over: they go on running.
        let adopter = self
            .groups
            .iter()
            .any(|other| other.id != group.id && other.key.host() == &ssh.host.id && other.live_connection().is_some());
        let forward = group.forward.as_ref()?;
        let running = forward.forwarder.running_rules();
        if adopter || running == 0 {
            return None;
        }
        let (title, text) = model::close_confirmation(&ssh.host.name, running, forward.forwarder.active_connections());
        Some((session.id, title, text))
    }

    // ---- review hooks ------------------------------------------------------------------------

    /// Debug review hook: one step of a forwarding scenario (see `demo::run`, `forward:`).
    #[cfg(debug_assertions)]
    pub(crate) fn demo_forward_step(&mut self, step: &str, arg: &str, window: &mut Window, cx: &mut Context<Self>) {
        use shellmate_forward::RuleState;

        use super::Panel;

        let Some(ix) = self.active else { return };
        match step {
            "show" => self.show_panel(Panel::Forward, window, cx),
            "terminal" => self.show_panel(Panel::Terminal, window, cx),
            _ => {}
        }
        let Some(session) = self.sessions.get(ix) else { return };
        let (host, group_id, session_id) = match session.ssh() {
            Some(ssh) => (ssh.host.clone(), ssh.group, session.id),
            None => return,
        };
        let panel = session.forward.as_ref().map(|tab| tab.panel.clone());
        match step {
            "show" | "terminal" => {}
            // The new-rule form, with `arg` as `kind|name|listen|target` typed in.
            "form" => {
                let mut parts = arg.splitn(4, '|');
                let kind = match parts.next() {
                    Some("R") => shellmate_forward::ForwardKind::Remote,
                    Some("D") => shellmate_forward::ForwardKind::Dynamic,
                    _ => shellmate_forward::ForwardKind::Local,
                };
                let values = crate::forward::model::RuleForm {
                    kind,
                    name: parts.next().unwrap_or_default().to_string(),
                    listen: parts.next().unwrap_or_default().to_string(),
                    target: parts.next().unwrap_or_default().to_string(),
                };
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.open_form(values, window, cx));
                }
            }
            // The form submitted as it stands (errors get shown).
            "submit-form" => {
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.demo_submit(window, cx));
                }
            }
            // Cards made up for the picture: `arg` names the set.
            "cards" => {
                let view = crate::forward::demo::view(arg, &host.name);
                if let Some(panel) = panel {
                    panel.update(cx, |panel, cx| panel.set_view(view, cx));
                }
            }
            // Switches a rule of this host on or off by name.
            "toggle" | "toggle-on" | "toggle-off" => {
                let target = self
                    .forward_owner(&host.id)
                    .and_then(|owner| self.group(owner))
                    .and_then(|g| g.forward.as_ref())
                    .and_then(|f| {
                        f.rules.iter().find(|rule| rule.name == arg).map(|rule| (rule.id.clone(), rule.enabled))
                    });
                if let Some((id, enabled)) = target {
                    let on = match step {
                        "toggle-on" => true,
                        "toggle-off" => false,
                        _ => !enabled,
                    };
                    self.toggle_rule(&host.id, &id, on, window, cx);
                }
            }
            // Retries a rule that could not take its port, with the port it suggests.
            "retry" => {
                let target = self.forward_view(group_id, &host).cards.into_iter().find(|card| card.name == arg);
                if let Some((id, action)) = target.and_then(|card| card.retry.map(|retry| (card.id, retry.action))) {
                    self.retry_rule(&host.id, &id, action, window, cx);
                }
            }
            // The question before the last tab closes, as the click on × would show it.
            "close" => self.close(ix, window, cx),
            // The deletion question of a rule by name.
            "delete" => {
                let target = self.forward_view(group_id, &host).cards.into_iter().find(|card| card.name == arg);
                if let Some(card) = target {
                    self.confirm_delete_rule(&host.id, &card.id, window, cx);
                }
            }
            // A second connection to the host (as ⌘⇧T makes one when the first cannot be shared).
            "second" => self.demo_second_connection(window, cx),
            // The first tab of the host closed, as × does (with the question, if one is due).
            "close-first" => {
                if let Some(first) =
                    self.sessions.iter().position(|s| s.ssh().is_some_and(|ssh| ssh.host.id == host.id))
                {
                    self.close(first, window, cx);
                }
            }
            // Closes the question that is open (as 取消 would).
            "dismiss" => window.close_all_dialogs(cx),
            // The tab closed for good, without asking.
            "close-now" => self.close_session(session_id, window, cx),
            // Logs how the rules stand (and how many connections the engine has).
            "log" => {
                let owner = self.forward_owner(&host.id).and_then(|owner| self.group(owner));
                if let Some(forward) = owner.and_then(|owner| owner.forward.as_ref()) {
                    let states: Vec<String> = forward
                        .forwarder
                        .views()
                        .iter()
                        .map(|view| {
                            let state = match &view.state {
                                RuleState::Running { port } => format!("运行中:{port}"),
                                other => format!("{other:?}"),
                            };
                            format!(
                                "{}={} ↓{} ↑{} 连接{}",
                                view.rule.name, state, view.stats.down, view.stats.up, view.stats.active
                            )
                        })
                        .collect();
                    log::info!("端口转发 [{arg}] {}", states.join(" | "));
                }
            }
            other => log::warn!("unknown forward step {other:?}"),
        }
    }
}
