use super::*;
pub(super) fn refresh_catalog(cx: &App) {
    let Some(hub) = cx.try_global::<Hub>() else {
        return;
    };
    let mut entries = vec![];
    for window in Hub::windows(cx) {
        if let Some(ws) = window.workspace.upgrade() {
            let ws = ws.read(cx);
            for s in &ws.sessions {
                if let Some(binding) = ws.ai_binding(Some(s.id), cx) {
                    entries.push((
                        binding,
                        s.display_title(cx).to_string(),
                        s.host().map_or("本机终端", |h| h.protocol.label()).into(),
                        s.host().map(|h| h.id.to_string()),
                        format!("窗口 {}", window.number),
                    ));
                }
            }
        }
    }
    hub.services.catalog.update(entries);
}
pub(super) fn session_catalog(cx: &mut App) -> crate::ai::session_catalog::Catalog {
    let (tx, rx) = async_channel::unbounded();
    let catalog = crate::ai::session_catalog::Catalog::new(tx);
    cx.spawn(async move |cx| {
        while let Ok(event) = rx.recv().await {
            cx.update(|cx| match event {
                crate::ai::host::Ui::ReadSession {
                    session,
                    lease,
                    lines,
                    reply,
                } => {
                    let result = hub::Route::Session(session)
                        .target(cx)
                        .and_then(|owner| {
                            owner.update(cx, |ws, _, cx| {
                                let binding = ws
                                    .ai_binding(Some(session), cx)
                                    .ok_or_else(|| "终端不存在".to_owned())?;
                                if !Arc::ptr_eq(&lease, &binding.alive)
                                    || !lease.load(std::sync::atomic::Ordering::Acquire)
                                {
                                    return Err("终端连接已变化".into());
                                }
                                let s = ws.sessions.iter().find(|s| s.id == session).unwrap();
                                let raw=s.terminal.read(cx).recent_displayed_text(lines.saturating_add(1).min(501));
                                let mut text=shellmate_ai::recent_context(&raw,lines.min(500));
                                let truncated=raw.lines().count()>lines || text.len()>64*1024;
                                if text.len()>64*1024 {let mut offset=text.len()-64*1024;while !text.is_char_boundary(offset){offset+=1;}text=text[offset..].into();}
                                if truncated{text=format!("[较早输出已省略]\n{text}");}
                                Ok(shellmate_ai::ContextSnapshot {
                                    label: s.display_title(cx).to_string(),
                                    protocol: s
                                        .host()
                                        .map_or("本机终端", |h| h.protocol.label())
                                        .into(),
                                    text,
                                })
                            })
                        })
                        .unwrap_or_else(|| Err("终端所在窗口已关闭".into()));
                    let _ = reply.send(result);
                }
                crate::ai::host::Ui::TerminalStart {
                    session,
                    command,
                    nonce,
                    spool,
                    lease,
                    reply,
                } => {
                    let result = hub::Route::Session(session)
                        .target(cx)
                        .and_then(|owner| {
                            owner.update(cx, |ws, _, cx| {
                                ws.ai_terminal_start(session, &command, &nonce, &lease, spool, cx)
                            })
                        })
                        .unwrap_or_else(|| Err(Box::new(shellmate_ai::ToolOutcome::Disconnected)));
                    let _ = reply.send(result);
                }
                crate::ai::host::Ui::TerminalFinish { session, nonce, .. } => {
                    if let Some(owner) = hub::Route::Session(session).target(cx) {
                        owner.update(cx, |ws, _, cx| {
                            ws.ai_terminal_finish(session, &nonce, true, cx)
                        });
                    }
                }
                _ => {}
            });
        }
    })
    .detach();
    catalog
}
impl Workspace {
    pub(crate) fn close_ai_conversation(&mut self,id:&str,w:&mut Window,cx:&mut Context<Self>) {
        self.hub.conversations.remove(id);
        self.ai_views.remove(id);
        if self.ai_drawer.as_ref().is_some_and(|d|d.read(cx).conversation_id()==id){self.ai_drawer=None;self.open_ai(w,cx);}
        cx.notify();
    }
    pub(crate) fn ai_drafts(&self,cx:&App)->Vec<(String,String)> {
        self.ai_views.iter().filter(|(id,d)|self.hub.conversations.get(id).is_none()&&!d.read(cx).runtime_retired()).map(|(id,d)|(id.clone(),d.read(cx).draft_title(cx))).collect()
    }
    pub(crate) fn new_ai_conversation(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self
            .ai_drawer
            .as_ref()
            .is_some_and(|d| d.read(cx).empty_draft(cx))
        {
            if let Some(drawer)=&self.ai_drawer{drawer.update(cx,|s,_|s.reuse_draft());}
            self.sync_ai_context(cx);
            return;
        }
        if let Some(old) = self.ai_drawer.take() {
            let id = old.read(cx).conversation_id().to_owned();
            self.ai_views.insert(id, old);
        }
        self.open_ai(w, cx);
    }
    pub(crate) fn locate_ai_terminal(&mut self,id:usize,_w:&mut Window,cx:&mut Context<Self>){cx.defer(move|cx|{if let Some(owner)=hub::Route::Session(id).target(cx){owner.update(cx,|s,w,cx|{if let Some(ix)=s.sessions.iter().position(|s|s.id==id){s.activate(ix,w,cx);w.activate_window();}});}});}
    pub(crate) fn select_ai_conversation(
        &mut self,
        id: &str,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .ai_drawer
            .as_ref()
            .is_some_and(|d| d.read(cx).conversation_id() == id)
        {
            return;
        }
        if let Some(old) = self.ai_drawer.take() {
            let id = old.read(cx).conversation_id().to_owned();
            self.ai_views.insert(id, old);
        }
        if let Some(view) = self.ai_views.remove(id) && !view.read(cx).runtime_retired() {
            self.ai_drawer = Some(view);
            self.ai_open = true;
            cx.notify();
            return;
        }
        self.open_ai(w, cx);
        let runtime = self.hub.conversations.get(id);
        if let Some(drawer) = &self.ai_drawer {
            drawer.update(cx, |d, cx| {
                if let Some(runtime) = runtime {
                    d.attach_runtime(runtime, cx);
                } else {
                    d.load_saved(id, cx);
                }
            });
        }
    }
    #[cfg(debug_assertions)]
    pub(crate) fn debug_focus_ai_composer(&self, window: &mut Window, cx: &mut App) {
        if self.ai_open
            && let Some(drawer) = &self.ai_drawer
        {
            drawer.update(cx, |drawer, cx| drawer.debug_focus_composer(window, cx));
        }
    }
    #[cfg(debug_assertions)]
    pub(crate) fn debug_ai_viewport_at(
        &self,
        position: gpui::Point<gpui::Pixels>,
        cx: &App,
    ) -> Option<gpui::ScrollHandle> {
        if !self.ai_open {
            return None;
        }
        self.ai_drawer
            .as_ref()?
            .read(cx)
            .debug_viewport_at(position, cx)
    }

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
        let local = matches!(session.kind, SessionKind::Local);
        Some(crate::ai::exec::Binding {
            session: session.id,
            connection,
            local,
            alive,
            capabilities: shellmate_ai::agent::Capabilities {
                web: false,
                sessions: false,
                background,
                terminal: can_terminal,
                terminal_busy: terminal.agent_busy(),
                cwd: crate::ai::exec::session_cwd(local, terminal.child_pid(), terminal.cwd()),
            },
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn ai_terminal_start(
        &mut self,
        id: usize,
        command: &str,
        nonce: &str,
        lease: &Arc<std::sync::atomic::AtomicBool>,
        spool: shellmate_core::ai_output::Spool,
        cx: &mut Context<Self>,
    ) -> Result<shellmate_term::agent::CaptureHandle, Box<shellmate_ai::agent::ToolOutcome>> {
        use shellmate_ai::agent::ToolOutcome;
        if !self
            .ai_binding(Some(id), cx)
            .is_some_and(|b| b.capabilities.terminal)
        {
            return Err(Box::new(ToolOutcome::Disconnected));
        }
        let session = self
            .sessions
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Box::new(ToolOutcome::Disconnected))?;
        if !Arc::ptr_eq(lease, &session.terminal.read(cx).agent_liveness())
            || !lease.load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(Box::new(ToolOutcome::Disconnected));
        }
        session
            .terminal
            .update(cx, |t, _| t.agent_start_spooled(command, nonce, spool))
            .map_err(|e| {
                Box::new({
                    if e == "busy" {
                        ToolOutcome::Busy
                    } else {
                        ToolOutcome::Disconnected
                    }
                })
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
                text: shellmate_ai::recent_context(
                    &s.terminal.read(cx).recent_displayed_text(usize::MAX),
                    40,
                ),
            },
        )
    }
    pub(crate) fn sync_ai_context(&mut self, cx: &mut Context<Self>) {
        let window = Hub::entry(self.handle, cx).map(|e| format!("窗口 {}", e.number)).unwrap_or_else(|| "当前窗口".into());
        let entries = self
            .sessions
            .iter()
            .filter_map(|s| {
                self.ai_binding(Some(s.id), cx).map(|binding| {
                    (
                        binding,
                        s.display_title(cx).to_string(),
                        s.host().map_or("本机终端", |h| h.protocol.label()).into(),
                        s.host().map(|h| h.id.to_string()),
                        window.clone(),
                    )
                })
            })
            .collect();
        self.hub.catalog.update_window(&window, entries);
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
            let db = self.hub.store.clone();
            let credentials = self.hub.secrets.clone();
            let data_root = self.hub.data_root.borrow().clone();
            self.ai_drawer = Some(cx.new(|cx| {
                crate::ai::panel::Drawer::new(workspace, db, credentials, data_root, w, cx)
            }));
        }
        self.ai_open = true;
        self.commands_open = false;
        self.sync_ai_context(cx);
        cx.notify();
    }
    pub(crate) fn clear_ai_memory(&mut self, cx: &mut Context<Self>) {
        self.hub.conversations.clear();
        self.ai_views.clear();
        if let Some(drawer) = &self.ai_drawer {
            drawer.update(cx, |d, cx| d.discard_and_clear(cx));
        }
        // The existing clear-all action deletes the entire conversation library.
        let outputs = self.hub.data_root.borrow().join("ai-outputs");
        if let Err(e) = std::fs::remove_dir_all(outputs)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("remove AI output library: {e}");
        }
    }
    pub(crate) fn close_ai(&mut self, cx: &mut Context<Self>) {
        self.end_drawer_drag(cx);
        self.ai_open = false;
        if let Some(drawer) = &self.ai_drawer {
            drawer.update(cx, |d, cx| d.close(cx));
        }
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

impl Workspace {
    pub(crate) fn memory_root(&self) -> std::path::PathBuf {
        self.hub.data_root.borrow().clone()
    }
    pub(crate) fn memory_host(&self) -> Option<String> {
        let session = self.focused_session()?;
        if matches!(session.kind, SessionKind::Local) {
            return Some("local".into());
        }
        let host = session.host()?;
        self.hub
            .book
            .borrow()
            .host(&host.id)
            .map(|h| h.id.to_string())
    }
    pub(crate) fn open_memory_manager(
        &self,
        focus: Option<crate::ai::memory_model::Focus>,
        w: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<crate::ai::memory_manager::MemoryManager>, String> {
        let current = self.memory_host();
        let focus =
            focus.unwrap_or_else(|| crate::ai::memory_model::Focus::current(current.as_deref()));
        crate::ai::memory_manager::MemoryManager::open(
            self.memory_root(),
            self.hub.store.clone(),
            current.as_deref(),
            focus,
            w,
            cx,
        )
    }
    pub(crate) fn save_ai_host_note(&self, id: &HostId, text: &str) -> Result<(), String> {
        crate::ai::memory_model::save_host_note(
            &self.memory_root(),
            &self.hub.store.borrow(),
            id,
            text,
        )
    }
}
