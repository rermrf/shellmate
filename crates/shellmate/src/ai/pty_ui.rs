//! Hidden terminal entities with an optional viewer. Closing the viewer never closes the PTY.
use super::{
    pty_tasks::{Pool, Record},
    pty_tools::CreateRequest,
    session_catalog::Catalog,
    terminal_interaction::{Operation, Reply},
};
use crate::theme::ActivePalette;
use gpui::{prelude::*, *};
use shellmate_term::{Terminal, TerminalEvent, TerminalView};
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak, atomic::Ordering},
};
struct Entry {
    terminal: Entity<Terminal>,
    record: Arc<Record>,
    pool: Weak<Pool>,
    window: Option<AnyWindowHandle>,
    _subscription: Subscription,
}
pub struct Registry {
    catalog: Catalog,
    entries: BTreeMap<usize, Entry>,
}
#[derive(Clone)]
struct GlobalRegistry(Entity<Registry>);
impl Global for GlobalRegistry {}
pub fn init(catalog: Catalog, cx: &mut App) {
    let registry = cx.new(|_| Registry {
        catalog,
        entries: BTreeMap::new(),
    });
    cx.set_global(GlobalRegistry(registry));
}
fn registry(cx: &App) -> Option<Entity<Registry>> {
    cx.try_global::<GlobalRegistry>().map(|g| g.0.clone())
}
pub fn contains(id: usize, cx: &App) -> bool {
    registry(cx).is_some_and(|r| r.read(cx).entries.contains_key(&id))
}
pub fn create(
    request: CreateRequest,
    cx: &mut App,
) -> Result<shellmate_ai::session_tools::Destination, String> {
    registry(cx)
        .ok_or("终端管理器不可用")?
        .update(cx, |r, cx| r.create(request, cx))
}
pub fn stop(id: usize, close: bool, cx: &mut App) {
    if let Some(r) = registry(cx) {
        r.update(cx, |r, cx| r.stop(id, close, cx));
    }
}
pub fn manage(
    spec: shellmate_ai::session_tools::Manage,
    owner: &shellmate_core::terminal_interaction::Controller,
    lease: &Arc<std::sync::atomic::AtomicBool>,
    cx: &mut App,
) -> Result<(), String> {
    let registry = registry(cx).ok_or("终端管理器不可用")?;
    registry.update(cx, |r, cx| {
        let e = r.catalog.lookup(&spec.session_id)?;
        let entry = r.entries.get(&e.binding.session).ok_or("独立终端已关闭")?;
        if !owner.active() || !Arc::ptr_eq(lease, &e.lease) {
            return Err("任务或终端身份已变化".into());
        }
        match spec.action {
            shellmate_ai::session_tools::ManageAction::Stop => r.stop(e.binding.session, false, cx),
            shellmate_ai::session_tools::ManageAction::Close => r.stop(e.binding.session, true, cx),
            shellmate_ai::session_tools::ManageAction::Resize => {
                let (cols, rows) = spec
                    .columns
                    .zip(spec.rows)
                    .filter(|(c, r)| shellmate_ai::session_tools::valid_size(*c, *r))
                    .ok_or("终端尺寸无效")?;
                entry.terminal.update(cx, |t, cx| {
                    t.set_size(shellmate_term::TerminalSize {
                        cols,
                        rows,
                        ..t.size()
                    });
                    cx.notify();
                });
            }
        };
        Ok(())
    })
}
pub fn interaction(
    session: usize,
    lease: &Arc<std::sync::atomic::AtomicBool>,
    owner: Arc<shellmate_core::terminal_interaction::Controller>,
    op: Operation,
    cx: &mut App,
) -> Result<Reply, shellmate_core::terminal_interaction::InteractionError> {
    let r = registry(cx).ok_or("终端管理器不可用")?;
    let r = r.read(cx);
    let e = r.entries.get(&session).ok_or("终端已关闭")?;
    if !owner.active() || !Arc::ptr_eq(lease, &e.record.endpoint.binding.alive) {
        return Err("任务或终端身份已变化".into());
    }
    if e.record.stopped.load(Ordering::Acquire)
        && matches!(op, Operation::Input { .. } | Operation::Check { .. })
    {
        return Err("程序已停止".into());
    }
    let terminal = e.terminal.clone();
    terminal.update(cx, |t, cx| {
        super::terminal_interaction::apply(t, op, owner, cx)
    })
}
pub fn read(
    session: usize,
    lease: &Arc<std::sync::atomic::AtomicBool>,
    lines: usize,
    cx: &App,
) -> Result<shellmate_ai::ContextSnapshot, String> {
    let r = registry(cx).ok_or("终端管理器不可用")?;
    let e = r.read(cx).entries.get(&session).ok_or("终端已关闭")?;
    if !Arc::ptr_eq(lease, &e.record.endpoint.binding.alive) {
        return Err("终端身份已变化".into());
    }
    Ok(shellmate_ai::ContextSnapshot {
        label: e.record.endpoint.target.label.clone(),
        protocol: e.record.endpoint.target.protocol.clone(),
        text: shellmate_ai::recent_context(
            &e.terminal.read(cx).recent_displayed_text(lines.min(500)),
            lines.min(500),
        ),
    })
}
pub fn available(reference: &str, cx: &App) -> bool {
    registry(cx).is_some_and(|r| {
        r.read(cx)
            .entries
            .values()
            .any(|e| e.record.endpoint.target.session_id == reference)
    })
}
pub fn show(reference: &str, cx: &mut App) -> bool {
    let Some(reg) = registry(cx) else {
        return false;
    };
    let Some((id, terminal, record, window)) = reg
        .read(cx)
        .entries
        .iter()
        .find(|(_, e)| e.record.endpoint.target.session_id == reference)
        .map(|(id, e)| (*id, e.terminal.clone(), e.record.clone(), e.window))
    else {
        return false;
    };
    if let Some(window) = window
        && window.update(cx, |_, w, _| w.activate_window()).is_ok()
    {
        return true;
    }
    let label = record.endpoint.target.label.clone();
    let result = gpui::open_window(
        WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(format!("{label} — Shellmate").into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(900.), px(620.)),
                cx,
            ))),
            window_min_size: Some(size(px(500.), px(320.))),
            ..Default::default()
        },
        cx,
        move |w, cx| {
            let style = cx.terminal_style();
            let view = cx.new(|cx| {
                let mut v = TerminalView::new(terminal, style.clone(), cx);
                v.hold_grid_size = true;
                v
            });
            view.focus_handle(cx).focus(w, cx);
            cx.new(|cx| {
                let sub = cx.observe(&view, |_, _, cx| cx.notify());
                Viewer {
                    id,
                    view,
                    record,
                    style,
                    _subscription: sub,
                }
            })
        },
    );
    if let Ok((window, _)) = result {
        reg.update(cx, |r, _| {
            if let Some(e) = r.entries.get_mut(&id) {
                e.window = Some(window);
            }
        });
        true
    } else {
        false
    }
}
impl Registry {
    fn create(
        &mut self,
        mut request: CreateRequest,
        cx: &mut Context<Self>,
    ) -> Result<shellmate_ai::session_tools::Destination, String> {
        let current = self.catalog.resolve(&request.parent.target.session_id)?;
        if !Arc::ptr_eq(&current.lease, &request.parent.lease)
            || !request.owner.active()
            || request.pool.retired.load(Ordering::Acquire)
        {
            return Err("来源连接或任务已变化".into());
        }
        if request.pool.list().len() >= 8 || self.entries.len() >= 32 {
            return Err("独立终端数量已达上限，请先关闭不用的终端".into());
        }
        let palette = cx.terminal_style().palette;
        let terminal = cx.new(|cx| {
            let mut t = Terminal::new(palette, cx);
            t.set_managed_program();
            t
        });
        let cwd = request.spec.cwd.clone().ok_or("目录未知")?;
        terminal.update(cx, |t, _| {
            t.set_size(shellmate_term::TerminalSize {
                cols: request.spec.columns,
                rows: request.spec.rows,
                ..Default::default()
            })
        });
        let attached = if let Some(shell) = request.shell.take() {
            terminal.update(cx, |t, cx| {
                t.attach(|sink, size| Ok(Box::new(shell.start(sink, size))), cx)
            })
        } else {
            let shell = std::env::var("SHELL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/bin/zsh".into());
            let program = shellmate_core::Program {
                executable: shell.into(),
                args: vec![
                    "-lc".into(),
                    format!(
                        "cd -- '{}' && {}",
                        cwd.replace('\'', "'\\''"),
                        request.spec.command
                    ),
                ],
                cwd: Some(cwd.clone().into()),
                env: [
                    ("TERM".into(), shellmate_core::Secret::new("xterm-256color")),
                    ("COLORTERM".into(), shellmate_core::Secret::new("truecolor")),
                ]
                .into(),
            };
            terminal.update(cx, |t, cx| {
                t.attach(
                    |sink, size| {
                        let exit = sink.clone();
                        Ok(Box::new(shellmate_term::LocalPty::spawn_owned_program(
                            &program,
                            size,
                            move |b| sink.output(b),
                            move |code| exit.exited(code),
                        )?))
                    },
                    cx,
                )
            })
        };
        if !attached {
            return Err("独立交互程序启动失败".into());
        }
        terminal.update(cx, |t, cx| {
            if t.agent_connected() {
                t.claim_interaction(request.owner, cx)?;
            }
            Ok::<_, String>(())
        })?;
        let mut endpoint = request.parent;
        endpoint.binding.alive = terminal.read(cx).agent_liveness();
        endpoint.binding.capabilities.cwd = Some(cwd.clone());
        endpoint.binding.capabilities.terminal_busy = true;
        endpoint.target.cwd = Some(cwd);
        endpoint.target.label = request.spec.purpose.chars().take(60).collect();
        endpoint.window = "独立终端".into();
        let endpoint = self.catalog.insert_independent(endpoint)?;
        let id = endpoint.binding.session;
        let record = match request.pool.register(
            endpoint.clone(),
            request.spec.command,
            request.call_id,
            request.source,
            self.catalog.bridge.clone(),
        ) {
            Ok(r) => r,
            Err(e) => {
                self.catalog.remove_independent(id);
                return Err(e);
            }
        };
        let record_for_events = record.clone();
        let sub = cx.subscribe(&terminal, move |_, _, event, cx| {
            let status = match event {
                TerminalEvent::Exited(code) => {
                    Some(super::tasks::Status::Exited(code.map(|n| n as i32)))
                }
                TerminalEvent::Disconnected(_) => Some(super::tasks::Status::Disconnected),
                _ => None,
            };
            if let Some(status) = status {
                let mut info = record_for_events.info.lock().unwrap();
                if info.status == super::tasks::Status::Running {
                    info.status = status;
                }
                cx.notify();
            }
        });
        self.entries.insert(
            id,
            Entry {
                terminal,
                record,
                pool: Arc::downgrade(&request.pool),
                window: None,
                _subscription: sub,
            },
        );
        Ok(endpoint.target)
    }
    fn stop(&mut self, id: usize, close: bool, cx: &mut Context<Self>) {
        if let Some(e) = self.entries.get(&id) {
            e.record.stopped.store(true, Ordering::Release);
            {
                let mut info = e.record.info.lock().unwrap();
                if info.status == super::tasks::Status::Running {
                    info.status = super::tasks::Status::Stopped;
                }
            }
            e.terminal
                .update(cx, |t, cx| t.stop_interactive_program(cx));
            if close {
                if let Some(w) = e.window {
                    let _ = w.update(cx, |_, w, _| w.remove_window());
                }
                if let Some(pool) = e.pool.upgrade() {
                    pool.remove(&e.record.endpoint.target.session_id);
                }
                self.catalog.remove_independent(id);
                self.entries.remove(&id);
            }
        }
        cx.notify();
    }
}
struct Viewer {
    style: shellmate_term::TerminalStyle,
    id: usize,
    view: Entity<TerminalView>,
    record: Arc<Record>,
    _subscription: Subscription,
}
impl Render for Viewer {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let style = cx.terminal_style();
        if self.style != style {
            self.view.update(cx, |v, cx| v.set_style(style.clone(), cx));
            self.style = style;
        }
        let font = _w
            .text_system()
            .resolve_font(&self.style.font(false, false));
        let cell_width = _w
            .text_system()
            .advance(font, self.style.font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(px(8.));
        let line_height = (self.style.font_size * self.style.line_height).round();
        let terminal = self.view.read(cx).terminal().clone();
        let current = terminal.read(cx).size();
        let grid = shellmate_term::TerminalSize {
            cell_width,
            line_height,
            ..current
        };
        if grid != current {
            terminal.update(cx, |t, _| t.set_size(grid));
        }
        let p = cx.palette();
        let info = self.record.info.lock().unwrap().clone();
        let id = self.id;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.win)
            .text_color(p.tx)
            .child(
                div()
                    .px(px(16.))
                    .py(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .text_size(px(13.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(info.command.clone()),
                            )
                            .child(div().text_size(px(11.)).text_color(p.tx3).child(format!(
                                "{} · {}",
                                self.record.endpoint.target.protocol,
                                info.status_text()
                            ))),
                    )
                    .when(info.status == super::tasks::Status::Running, |d| {
                        d.child(
                            div()
                                .id("pty-stop")
                                .cursor_pointer()
                                .child("停止程序")
                                .on_click(move |_, _, cx| stop(id, false, cx)),
                        )
                    })
                    .child(
                        div()
                            .id("pty-close")
                            .cursor_pointer()
                            .child(if info.status == super::tasks::Status::Running {
                                "停止并关闭"
                            } else {
                                "关闭终端"
                            })
                            .on_click(move |_, w, cx| {
                                stop(id, true, cx);
                                w.remove_window();
                            }),
                    )
                    .child(
                        div()
                            .id("pty-hide")
                            .cursor_pointer()
                            .child("收起")
                            .on_click(|_, w, _| w.remove_window()),
                    ),
            )
            .child(
                div()
                    .id("pty-viewport")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .p(px(8.))
                    .child(
                        div()
                            .w(cell_width * f32::from(grid.cols) + px(36.))
                            .h(line_height * f32::from(grid.rows) + px(28.))
                            .child(self.view.clone()),
                    ),
            )
    }
}

/// Isolated native review only. The fixture shell uses /private/tmp and cat; no user files.
#[cfg(debug_assertions)]
pub fn fixture(pool: Arc<Pool>, cx: &mut App) -> shellmate_ai::session_tools::Destination {
    let registry = registry(cx).unwrap();
    let catalog = registry.read(cx).catalog.clone();
    let parent = super::session_catalog::Endpoint {
        independent: true,
        target: shellmate_ai::session_tools::Destination {
            session_id: String::new(),
            label: "临时本机".into(),
            protocol: "本机终端".into(),
            host_id: None,
            cwd: Some("/private/tmp".into()),
        },
        binding: super::exec::Binding {
            session: 0,
            local: true,
            connection: None,
            alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            capabilities: shellmate_ai::Capabilities {
                terminal: true,
                background: true,
                cwd: Some("/private/tmp".into()),
                ..Default::default()
            },
        },
        window: "测试".into(),
        connected: true,
        lease: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        slots: Arc::new(tokio::sync::Semaphore::new(4)),
    };
    let parent = catalog.insert_independent(parent).unwrap();
    let parent_id = parent.binding.session;
    let target=create(CreateRequest{parent,spec:shellmate_ai::session_tools::Create{session_id:String::new(),command:"printf 'Independent terminal ready\\nType text, or Ctrl-D to exit.\\n'; exec /bin/cat".into(),cwd:Some("/private/tmp".into()),purpose:"检查独立交互终端".into(),columns:100,rows:30},pool,owner:Arc::new(shellmate_core::terminal_interaction::Controller::new("测试 Agent".into())),call_id:"pty-fixture".into(),source:None,shell:None},cx).unwrap();
    catalog.remove_independent(parent_id);
    target
}
