//! Telnet and serial tabs share the terminal/productivity pipeline with SSH.
use super::*;
use gpui::component::button::Button;
use gpui::component::menu::{DropdownMenu as _, PopupMenuItem};
use shellmate_core::{Host, Protocol};
use shellmate_serial::{Command, Control, State};

pub(super) struct StreamSession {
    pub host: Host,
    pub connected: bool,
    pub serial: Option<Control>,
    pub snapshot: Option<State>,
    attempt: Option<tokio::task::AbortHandle>,
    task: Option<gpui::Task<()>>,
}
impl StreamSession {
    pub fn new(host: Host) -> Self {
        let snapshot = (host.protocol == Protocol::Serial).then(|| State::new(host.serial.clone()));
        Self {
            host,
            connected: false,
            serial: None,
            snapshot,
            attempt: None,
            task: None,
        }
    }
    pub fn connected(&self) -> bool {
        self.snapshot.as_ref().map_or(self.connected, |s| s.open)
    }
}
impl Drop for StreamSession {
    fn drop(&mut self) {
        if let Some(task) = &self.attempt {
            task.abort();
        }
    }
}
impl Workspace {
    pub(super) fn open_stream(&mut self, host: Host, window: &mut Window, cx: &mut Context<Self>) {
        let style = cx.terminal_style();
        let terminal = cx.new(|cx| Terminal::new(style.palette.clone(), cx));
        let view = cx.new(|cx| {
            TerminalView::new(terminal.clone(), style, cx)
                .with_search_icon(crate::icons::AppIcon::Search.at(14.))
        });
        let id = self.next_id;
        self.next_id += 1;
        let subscription = self.watch_terminal(id, &terminal, window, cx);
        self.sessions.push(Session {
            id,
            title: host.name.clone().into(),
            view,
            terminal,
            exited: false,
            panel: Panel::Terminal,
            files: None,
            forward: None,
            monitor: None,
            typed: None,
            kind: SessionKind::Stream(Box::new(StreamSession::new(host))),
            _subscription: subscription,
        });
        self.start_session_archives(id, window, cx);
        self.activate(self.sessions.len() - 1, window, cx);
        self.start_stream(id, window, cx);
    }
    pub(super) fn start_stream(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if crate::prototype_fixture() {
            return;
        }
        if self.secrets.is_locked() && self.sessions.iter().any(|s|s.id==id&&s.host().is_some_and(|h|h.protocol==Protocol::Telnet)) {
            let this=cx.entity().downgrade();crate::security::ensure_unlocked(self.secrets.clone(),"连接 Telnet",window,cx,move|ok,w,cx|{if ok{this.update(cx,|s,cx|s.start_stream(id,w,cx)).ok();}});return;
        }
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) else {
            return;
        };
        let SessionKind::Stream(stream) = &mut session.kind else {
            return;
        };
        let host = stream.host.clone();
        let terminal = session.terminal.clone();
        if host.protocol == Protocol::Serial {
            let mut control = None;
            terminal.update(cx, |t, cx| {
                t.attach(
                    |sink, _| {
                        let (backend, c) = shellmate_serial::Channel::start(
                            host.addr.clone(),
                            host.serial.clone(),
                            sink,
                        );
                        control = Some(c);
                        Ok(Box::new(backend))
                    },
                    cx,
                )
            });
            stream.serial = control;
            return;
        }
        let Some(runtime) = crate::runtime::get(cx) else {
            return;
        };
        if let Some(abort) = stream.attempt.take() {
            abort.abort();
        }
        stream.connected = false;
        let secrets = self.secrets.clone();
        let endpoint = connect::Endpoint::of(&host, &connect::default_user());
        let form = self
            .pending_forms
            .get(&host.id)
            .filter(|(e, _)| *e == endpoint)
            .map(|(_, f)| f.answer.secret.clone());
        let worker_host = host.clone();
        let join = runtime.spawn(async move {
            let password = match form {
                Some(secret) => Some(secret),
                None => secrets
                    .get(&shellmate_store::SecretKey::HostPassword(
                        worker_host.id.clone(),
                    ))
                    .map_err(|e| e.to_string())?,
            };
            let socket = shellmate_telnet::connect(&worker_host.addr, worker_host.port).await?;
            Ok::<_, String>((socket, password))
        });
        stream.attempt = Some(join.abort_handle());
        stream.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            this.update_in(cx, |this, window, cx| {
                let Some(session) = this.sessions.iter_mut().find(|s| s.id == id) else {
                    return;
                };
                let SessionKind::Stream(stream) = &mut session.kind else {
                    return;
                };
                stream.attempt = None;
                match result {
                    Ok(Ok((socket, password))) => {
                        terminal.update(cx, |t, cx| {
                            t.attach(
                                |sink, dims| {
                                    Ok(Box::new(shellmate_telnet::Channel::start(
                                        socket,
                                        sink,
                                        dims,
                                        host.user.clone().unwrap_or_default(),
                                        password,
                                        &runtime.handle(),
                                    )))
                                },
                                cx,
                            )
                        });
                        stream.connected = true;
                        session.exited = false;
                        let _ = this.store.borrow_mut().touch_connected(&host.id);
                    }
                    outcome => {
                        let reason = match outcome {
                            Ok(Err(e)) => e,
                            Err(_) => "连接已取消".into(),
                            _ => unreachable!(),
                        };
                        terminal.update(cx, |t, cx| {
                            t.attach(|_, _| Err(anyhow::anyhow!(reason.clone())), cx)
                        });
                        window.push_notification(Notification::error(reason), cx);
                    }
                }
                this.sync_sidebar(cx);
                cx.notify();
            })
            .ok();
        }));
    }
    pub(super) fn poll_streams(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        for session in &mut self.sessions {
            if let SessionKind::Stream(stream) = &mut session.kind
                && let Some(control) = &stream.serial
            {
                if let Some(notice) = control.take_notice() {
                    window.push_notification(Notification::info(notice), cx);
                }
                let state = control.state();
                if stream.snapshot.as_ref() != Some(&state) {
                    stream.connected = state.open;
                    stream.snapshot = Some(state);
                    changed = true;
                }
            }
        }
        if changed {
            self.sync_sidebar(cx);
            cx.notify();
        }
    }
    fn serial_command(
        &mut self,
        id: usize,
        command: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) else {
            return;
        };
        let SessionKind::Stream(stream) = &mut session.kind else {
            return;
        };
        if let Some(control) = &stream.serial
            && let Err(e) = control.send(command)
        {
            window.push_notification(Notification::error(e.to_string()), cx);
        }
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(180))
                .await;
            this.update_in(cx, |this, w, cx| this.poll_streams(w, cx))
                .ok();
        })
        .detach();
        cx.notify();
    }
    pub(super) fn render_serial_toolbar(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let SessionKind::Stream(stream) = &session.kind else {
            return None;
        };
        let state = stream.snapshot.as_ref()?;
        let id = session.id;
        let p = cx.palette().clone();
        let tp = cx.terminal_style().palette;
        let fg = to_hsla(tp.foreground);
        let dim = to_hsla(tp.dim);
        let neutral: gpui::Hsla = gpui::rgba(0x7f7f7f29).into();
        let action = |key: &'static str,
                      label: String,
                      active: bool,
                      command: Command,
                      cx: &mut Context<Self>| {
            div()
                .id((key, id))
                .h(px(24.))
                .flex_none()
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(7.))
                .bg(if active {
                    to_hsla(
                        tp.ansi[if key == "serial-dtr" || key == "serial-rts" {
                            2
                        } else {
                            6
                        }],
                    )
                } else {
                    neutral
                })
                .text_color(if active {
                    gpui::rgb(0x0c0d10).into()
                } else {
                    fg
                })
                .text_size(px(11.5))
                .line_height(px(14.))
                .cursor_pointer()
                .child(label)
                .on_click(cx.listener(move |this, _, w, cx| {
                    this.serial_command(id, command.clone(), w, cx)
                }))
        };
        let weak = cx.entity().downgrade();
        let baud = Button::new(("serial-baud", id))
            .ghost()
            .compact()
            .h(px(24.))
            .px(px(9.))
            .rounded(px(7.))
            .bg(neutral)
            .text_color(fg)
            .text_size(px(11.5))
            .label(format!("{} ▾", state.settings.label()))
            .dropdown_menu(move |mut menu, _, _| {
                for baud in [9600, 115200, 460800, 921600] {
                    let weak = weak.clone();
                    menu = menu.item(PopupMenuItem::new(baud.to_string()).on_click(
                        move |_, w, cx| {
                            weak.update(cx, |this, cx| {
                                this.serial_command(id, Command::Baud(baud), w, cx)
                            })
                            .ok();
                        },
                    ));
                }
                menu
            });
        let weak = cx.entity().downgrade();
        let reset = Button::new(("serial-reset", id))
            .ghost()
            .compact()
            .h(px(24.))
            .px(px(10.))
            .rounded(px(7.))
            .bg(neutral)
            .text_color(fg)
            .text_size(px(11.5))
            .label("复位 EN")
            .tooltip("复位 EN / 下载模式")
            .dropdown_menu(move |menu, _, _| {
                let reset = weak.clone();
                let menu = menu.item(PopupMenuItem::new("复位 EN").on_click(move |_, w, cx| {
                    reset
                        .update(cx, |this, cx| {
                            this.serial_command(id, Command::Reset, w, cx)
                        })
                        .ok();
                }));
                let weak = weak.clone();
                menu.item(PopupMenuItem::new("下载模式").on_click(move |_, w, cx| {
                    weak.update(cx, |this, cx| {
                        this.serial_command(id, Command::Download, w, cx)
                    })
                    .ok();
                }))
            });
        Some(
            div()
                .flex_none()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .py(px(10.))
                .border_b_1()
                .border_color(gpui::rgba(0x7f7f7f29))
                .font_family(theme::MONO_FONT)
                .text_size(px(12.))
                .line_height(px(15.))
                .text_color(dim)
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::BOLD)
                        .text_color(if state.open {
                            to_hsla(tp.ansi[2])
                        } else {
                            p.tx3
                        })
                        .child(state.status()),
                )
                .child(
                    crate::ui::ellipsis(("serial-device", id), stream.host.addr.clone())
                        .min_w_0()
                        .flex_shrink(1.)
                        .text_color(fg),
                )
                .child(baud)
                .child(
                    action(
                        "serial-newline",
                        format!("换行 {}", state.settings.newline.label()),
                        false,
                        Command::Newline(state.settings.newline.next()),
                        cx,
                    )
                    .bg(gpui::rgba(0x00000000))
                    .px(px(0.))
                    .text_color(dim),
                )
                .child(div().flex_1().min_w_0())
                .child(action(
                    "serial-hex",
                    "HEX".into(),
                    state.hex,
                    Command::Display {
                        hex: !state.hex,
                        timestamps: state.timestamps,
                    },
                    cx,
                ))
                .child(action(
                    "serial-time",
                    "时间戳".into(),
                    state.timestamps,
                    Command::Display {
                        hex: state.hex,
                        timestamps: !state.timestamps,
                    },
                    cx,
                ))
                .child(action(
                    "serial-dtr",
                    "DTR".into(),
                    state.dtr,
                    Command::Dtr(!state.dtr),
                    cx,
                ))
                .child(action(
                    "serial-rts",
                    "RTS".into(),
                    state.rts,
                    Command::Rts(!state.rts),
                    cx,
                ))
                .child(reset)
                .child(action(
                    "serial-open",
                    if state.open {
                        "关闭端口"
                    } else {
                        "打开端口"
                    }
                    .into(),
                    false,
                    Command::Open(!state.open),
                    cx,
                ))
                .into_any_element(),
        )
    }
}
