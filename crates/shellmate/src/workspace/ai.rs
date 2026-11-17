use super::*;
impl Workspace {
    pub(crate) fn ai_binding(
        &self,
        id: Option<usize>,
        cx: &App,
    ) -> Option<crate::ai::exec::Binding> {
        let session = self.sessions.iter().find(|s| Some(s.id) == id)?;
        let terminal = session.terminal.read(cx);
        let connection = session
            .ssh()
            .and_then(|s| self.group(s.group))
            .and_then(|g| g.live_connection());
        let connected = terminal.agent_connected()
            && match &session.kind {
                SessionKind::Local => !session.exited,
                SessionKind::Ssh(s) => s.status == ssh::SshStatus::Connected,
                SessionKind::Stream(s) => s.connected(),
            };
        let (background, can_terminal) = crate::ai::model::execution_targets(
            session.host().map(|h| h.protocol),
            connected,
            connection.is_some(),
        );
        let alive = if background && !terminal.agent_connected() {
            session.ai_alive.clone()
        } else {
            terminal.agent_liveness()
        };
        Some(crate::ai::exec::Binding {
            session: session.id,
            connection,
            local: matches!(session.kind, SessionKind::Local),
            alive,
            capabilities: shellmate_ai::agent::Capabilities {
                background,
                terminal: can_terminal,
                terminal_busy: terminal.agent_busy(),
                cwd: terminal.cwd().map(str::to_owned),
            },
        })
    }
    pub(crate) fn ai_terminal_start(
        &mut self,
        id: usize,
        command: &str,
        nonce: &str,
        lease: &Arc<std::sync::atomic::AtomicBool>,
        cx: &mut Context<Self>,
    ) -> Result<shellmate_term::agent::CaptureHandle, shellmate_ai::agent::ToolOutcome> {
        use shellmate_ai::agent::ToolOutcome;
        if !self
            .ai_binding(Some(id), cx)
            .is_some_and(|b| b.capabilities.terminal)
        {
            return Err(ToolOutcome::Disconnected);
        }
        let session = self
            .sessions
            .iter()
            .find(|s| s.id == id)
            .ok_or(ToolOutcome::Disconnected)?;
        if !Arc::ptr_eq(lease, &session.terminal.read(cx).agent_liveness())
            || !lease.load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(ToolOutcome::Disconnected);
        }
        session
            .terminal
            .update(cx, |t, _| t.agent_start(command, nonce))
            .map_err(|e| {
                if e == "busy" {
                    ToolOutcome::Busy
                } else {
                    ToolOutcome::Disconnected
                }
            })
    }
    pub(crate) fn ai_terminal_finish(
        &mut self,
        id: usize,
        nonce: &str,
        finish: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.sessions.iter().find(|s| s.id == id) {
            session.terminal.update(cx, |t, _| {
                if finish {
                    t.agent_finish(nonce, true)
                } else {
                    t.agent_interrupt(nonce)
                }
            });
        }
    }
    pub(crate) fn ai_context(&self, id: Option<usize>, cx: &App) -> shellmate_ai::ContextSnapshot {
        self.sessions.iter().find(|s| Some(s.id) == id).map_or_else(
            shellmate_ai::ContextSnapshot::default,
            |s| shellmate_ai::ContextSnapshot {
                label: s.title.to_string(),
                protocol: s.host().map_or("本机终端", |h| h.protocol.label()).into(),
                text: shellmate_ai::redact::recent_context(
                    &s.terminal.read(cx).recent_displayed_text(usize::MAX),
                    40,
                )
                .text,
            },
        )
    }
    pub(crate) fn sync_ai_context(&mut self, cx: &mut Context<Self>) {
        let Some(drawer) = self.ai_drawer.clone() else {
            return;
        };
        let session = self.focused_session();
        let id = session.map(|s| s.id);
        let host = session.and_then(|s| s.host().map(|h| h.id.to_string()));
        let label = session.map_or_else(|| "无会话".into(), |s| s.title.to_string());
        let protocol = session.map_or_else(String::new, |s| {
            s.host().map_or("本机终端", |h| h.protocol.label()).into()
        });
        let context = crate::ai::panel::SessionContext {
            session: id,
            host,
            label,
            protocol,
            binding: self.ai_binding(id, cx),
        };
        drawer.update(cx, |d, cx| d.sync_context(context, cx));
    }
    pub(super) fn open_ai(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.ai_drawer.is_none() {
            let workspace = cx.entity().downgrade();
            let db = self.store.clone();
            let credentials = self.secrets.clone();
            self.ai_drawer =
                Some(cx.new(|cx| crate::ai::panel::Drawer::new(workspace, db, credentials, w, cx)));
        }
        self.ai_open = true;
        self.commands_open = false;
        self.sync_ai_context(cx);
        cx.notify();
    }
    pub(crate) fn clear_ai_memory(&mut self, cx: &mut Context<Self>) {
        if let Some(drawer) = &self.ai_drawer {
            drawer.update(cx, |d, cx| d.clear(cx));
        }
    }
    pub(crate) fn close_ai(&mut self, cx: &mut Context<Self>) {
        self.end_drawer_drag(cx);
        self.ai_open = false;
        cx.notify();
    }
    pub(super) fn explain_ai(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let line = self.focused_session().and_then(|s| {
            crate::ai::model::error_line(
                &s.terminal
                    .read(cx)
                    .recent_displayed_text(s.terminal.read(cx).size().rows as usize),
            )
        });
        self.open_ai(w, cx);
        if let Some(line) = line {
            let drawer = self.ai_drawer.clone().unwrap();
            w.defer(cx, move |w, cx| {
                drawer.update(cx, |d, cx| d.ask(format!("解释这段报错：\n{line}"), w, cx))
            });
        } else {
            w.push_notification(Notification::info("当前可见终端中没有发现报错"), cx);
        }
    }
    pub(crate) fn open_ai_settings(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(5, w, cx);
    }
    pub(crate) fn ai_insert(
        &mut self,
        card: shellmate_ai::CommandCard,
        target: Option<usize>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.command_available(target, cx) {
            w.push_notification(
                Notification::info("目标会话已关闭或尚未连接，命令未插入"),
                cx,
            );
            return;
        }
        let request = self.send_request(target.into_iter().collect(), false, false);
        self.open_confirmation(
            shellmate_store::Snippet {
                id: String::new(),
                name: "AI 建议命令".into(),
                command: card.command,
            },
            request,
            w,
            cx,
        );
    }
}
