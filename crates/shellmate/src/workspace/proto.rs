//! Prototype scenes use real view entities with an in-memory library and inert I/O.
use super::*;
use crate::productivity::multi::Pane;
use crate::proto::{self, DATA};
use shellmate_core::{AuthMethod, Group, GroupId, GroupTone, Host, Protocol};
use shellmate_term::{OutputSink, TerminalBackend};
struct Inert;
impl TerminalBackend for Inert {
    fn write(&mut self, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn resize(&mut self, _: u16, _: u16, _: u16, _: u16) -> std::io::Result<()> {
        Ok(())
    }
    fn close(&mut self) {}
}
impl Workspace {
    pub(crate) fn proto_scene(&mut self, scene: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !proto::SCENES.contains(&scene) {
            log::warn!("unknown prototype scene");
            return;
        }
        self.sessions.clear();
        self.groups.clear();
        self.active = None;
        self.monitor_frozen = true;
        self.preferences = Default::default();
        self.store = Rc::new(RefCell::new(HostStore::open_in_memory().expect("fixture")));
        for (i, (name, tone)) in [
            ("生产环境", GroupTone::Accent),
            ("测试环境", GroupTone::Warn),
            ("个人", GroupTone::Ok),
            ("设备", GroupTone::Neutral),
        ]
        .into_iter()
        .enumerate()
        {
            self.store
                .borrow_mut()
                .save_group(&Group {
                    id: GroupId::from_raw(name),
                    name: name.into(),
                    tone,
                    sort: i as i64,
                })
                .unwrap();
        }
        let hosts = DATA["HOSTS"]
            .a()
            .iter()
            .map(|h| {
                let mut host = Host::new(h["name"].s(), h["addr"].s());
                host.id = HostId::from_raw(h["id"].s());
                host.user = (!h["user"].s().is_empty()).then(|| h["user"].s().into());
                host.port = if h["port"].n() > 0. {
                    h["port"].n() as u16
                } else {
                    22
                };
                host.group = Some(GroupId::from_raw(h["group"].s()));
                host.tags = h["tags"].a().iter().map(|t| t.s().to_string()).collect();
                host.favorite = h["fav"].b();
                host.protocol = match h["proto"].s() {
                    "Mosh" => Protocol::Mosh,
                    "串口" => Protocol::Serial,
                    "Telnet" => Protocol::Telnet,
                    _ => Protocol::Ssh,
                };
                host.auth = match h["auth"].s() {
                    "密码" => AuthMethod::Password,
                    "ssh-agent" => AuthMethod::Agent,
                    "键盘交互" => AuthMethod::KeyboardInteractive,
                    _ => AuthMethod::PrivateKey,
                };
                host.last_connected_at = if matches!(h["rt"], proto::V::Num(_)) {
                    Some(proto::now() - (h["rt"].n() * 60.) as i64)
                } else {
                    None
                };
                if !h["jump"].s().is_empty() {
                    host.jump = vec![HostId::from_raw("h4")];
                }
                host
            })
            .collect::<Vec<_>>();
        self.store.borrow_mut().save_hosts(&hosts).unwrap();
        self.reload_book(window, cx);
        self.store
            .borrow_mut()
            .save_snippets(&crate::productivity::model::samples())
            .unwrap();
        self.refresh_snippets(cx);
        for h in DATA["HIST"].a().iter().rev() {
            let label = h["h"].s();
            let host = self
                .book
                .hosts
                .iter()
                .find(|x| x.name == label)
                .map_or("local", |h| h.id.as_str())
                .to_string();
            let time = match h["t"].s() {
                "昨天" => proto::timestamp("11-11 09:41"),
                v if v.contains(':') => proto::timestamp(v),
                v => proto::timestamp(&format!("{v} 09:41")),
            };
            self.store
                .borrow_mut()
                .add_history(&shellmate_store::HistoryEntry {
                    id: 0,
                    host,
                    label: label.into(),
                    command: h["c"].s().into(),
                    cwd: Some("/var/www/app".into()),
                    time,
                })
                .unwrap();
        }
        let dark = cx.is_dark();

        for (i, (host_id, lines)) in [("h1", "S1"), ("h3", "S2"), ("h10", "S3"), ("h7", "S4")]
            .into_iter()
            .enumerate()
        {
            let host = self.book.host(&HostId::from_raw(host_id)).unwrap().clone();
            let terminal = cx.new(|cx| Terminal::new(cx.terminal_style().palette, cx));
            terminal.update(cx, |t, cx| {
                let mut size = t.size();
                size.cols = 160;
                size.rows = 40;
                t.set_size(size);
                let text = proto::ansi(&DATA[lines], dark);
                t.attach(
                    move |sink: OutputSink, _| {
                        sink.output(text.as_bytes());
                        Ok(Box::new(Inert))
                    },
                    cx,
                );
            });
            let view = cx.new(|cx| {
                let mut v = TerminalView::new(terminal.clone(), cx.terminal_style(), cx);
                v.top_radius = CARD_RADIUS;
                v
            });
            let id = i + 1;
            let subscription = self.watch_terminal(id, &terminal, window, cx);
            let mut ssh = ssh::SshSession::demo(host.clone());
            if i == 3 {
                ssh.status = ssh::SshStatus::Reconnecting(proto::reading(scene, dark).reconnect);
            }
            self.sessions.push(Session {
                id,
                title: host.name.into(),
                view,
                terminal,
                exited: false,
                panel: Panel::Terminal,
                files: None,
                forward: None,
                monitor: None,
                typed: None,
                kind: SessionKind::Ssh(Box::new(ssh)),
                _subscription: subscription,
            });
        }
        self.next_id = 5;
        self.activate(0, window, cx);
        self.broadcast.checked = [1, 2].into();
        self.sync_productivity(cx);
        match scene {
            "cmds" | "bc" | "qc" | "tabmenu" => {
                self.commands_open = true;
                if scene == "bc" {
                    self.set_broadcast(true, window, cx);
                }
                if scene == "qc" {
                    self.proto_confirm(window, cx);
                }
                if scene == "tabmenu" {
                    self.open_session_menu(
                        crate::productivity::session_menu::Kind::Tabs,
                        gpui::point(px(904.), px(52.)),
                        window,
                        cx,
                    );
                }
            }
            "pal" => self.open_palette_query("", window, cx),
            "pal-q" => self.open_palette_query("prod", window, cx),
            "hist" => self.open_history(window, cx),
            "split" => {
                self.split.other = Some(2);
                self.split.focus = Pane::Main;
                self.update_pane_radii(cx);
            }
            "rail" => self.set_sidebar_open(false, cx),
            "local" => self.proto_local(window, cx),
            "reconnect" => self.activate(3, window, cx),
            "files" | "editor" => self.proto_files(scene, window, cx),
            "fwd" | "fwd-new" => {
                let view = crate::forward::demo::view("cards", "prod-web-01");
                let panel = cx.new(|cx| crate::forward::panel::ForwardPanel::new(view, cx));
                let subscription =
                    cx.subscribe(&panel, |_, _, _: &crate::forward::panel::ForwardPanelEvent, _| {});
                if scene == "fwd-new" {
                    panel.update(cx, |p, cx| p.toggle_form(window, cx));
                }
                self.sessions[0].forward = Some(forward::ForwardTab {
                    panel,
                    _subscription: subscription,
                });
                self.sessions[0].panel = Panel::Forward;
            }
            "mon-ov" | "mon-proc" | "mon-svc" | "mon-dk" => self.proto_monitor(scene, window, cx),
            "rv" | "rv-log" => {
                let panel = crate::records::panel::RecordsWindow::open(
                    self.store.clone(),
                    PathBuf::from("/nonexistent-shellmate-fixture"),
                    None,
                    false,
                    Default::default(),
                    window,
                    cx,
                );
                panel.update(cx, |r, cx| r.proto_scene(scene == "rv-log", cx));
                self.records_window = Some(panel.downgrade());
            }
            "host-new" => self.new_host(window, cx),
            "hostkey-new" | "hostkey-changed" => self.proto_hostkey(scene, window, cx),
            "set-cred" => self.open_settings(0, window, cx),
            "set-sync" => self.open_settings(1, window, cx),
            "set-term" => self.open_settings(2, window, cx),
            "set-local" => self.open_settings(3, window, cx),
            "set-conn" => self.open_settings(4, window, cx),
            _ => {}
        }
        self.sync_sidebar(cx);
        cx.notify();
    }
    fn proto_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let request = self.send_request(vec![1], false, false);
        self.open_confirmation(
            shellmate_store::Snippet {
                name: "负载".into(),
                command: "uptime".into(),
            },
            request,
            window,
            cx,
        );
    }
    fn proto_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminal = cx.new(|cx| Terminal::new(cx.terminal_style().palette, cx));
        let palette = cx.terminal_style().palette;
        terminal.update(cx,|t,cx|{t.attach(move|sink,_|{let dim=palette.dim;let green=palette.ansi[2];let blue=palette.ansi[4];let text=format!("\x1b[38;2;{};{};{}mLast login: Thu Nov 12 09:12:40 on ttys002\r\n\x1b[1;38;2;{};{};{}mchen@MacBook-Pro\x1b[0m \x1b[1;38;2;{};{};{}m~\x1b[0m % \x1b[?25l",dim.r,dim.g,dim.b,green.r,green.g,green.b,blue.r,blue.g,blue.b);sink.output(text.as_bytes());Ok(Box::new(Inert))},cx);});
        let view = cx.new(|cx| TerminalView::new(terminal.clone(), cx.terminal_style(), cx));
        let id = 5;
        let subscription = self.watch_terminal(id, &terminal, window, cx);
        self.sessions.push(Session {
            id,
            title: "本机".into(),
            view,
            terminal,
            exited: false,
            panel: Panel::Terminal,
            files: None,
            forward: None,
            monitor: None,
            typed: None,
            kind: SessionKind::Local,
            _subscription: subscription,
        });
        self.next_id = 6;
        self.user_host = "chen@MacBook-Pro".into();
        self.activate(4, window, cx);
    }
}
impl Workspace {
    fn proto_monitor(&mut self, scene: &str, window: &mut Window, cx: &mut Context<Self>) {
        let data = crate::proto::monitor::data(scene, cx.is_dark());
        let panel =
            cx.new(|cx| crate::monitor::panel::MonitorPanel::new("prod-web-01", data, None, window, cx));
        let page = match scene {
            "mon-proc" => crate::monitor::model::Page::Processes,
            "mon-svc" => crate::monitor::model::Page::Services,
            "mon-dk" => crate::monitor::model::Page::Docker,
            _ => crate::monitor::model::Page::Overview,
        };
        panel.update(cx, |p, cx| {
            p.set_page(page, cx);
            if scene == "mon-proc" {
                p.show_process(2207, window, cx);
            }
            if scene == "mon-svc" {
                p.demo_services(
                    "",
                    crate::monitor::services::ServiceFilter::All,
                    Some("php8.2-fpm.service"),
                    window,
                    cx,
                );
                p.demo_service_log(crate::monitor::demo::service_log(), cx);
                p.proto_order(cx);
            }
            if scene == "mon-dk" {
                p.demo_container_log("worker", crate::monitor::demo::container_log(), cx);
            }
        });
        let subscription = cx.subscribe(&panel, |_, _, _: &crate::monitor::panel::MonitorPanelEvent, _| {});
        self.sessions[0].monitor = Some(monitor::MonitorTab {
            panel,
            page,
            _subscription: subscription,
        });
        self.sessions[0].panel = Panel::Monitor;
    }
    fn proto_files(&mut self, scene: &str, window: &mut Window, cx: &mut Context<Self>) {
        let terminal = self.sessions[0].terminal.clone();
        let Some(runtime) = crate::runtime::get(cx) else {
            return;
        };
        let panel = cx.new(|cx| crate::files::panel::FilesPanel::new(&terminal, runtime, window, cx));
        panel.update(cx, |p, cx| p.proto_fixture(scene, cx));
        let subscription = cx.subscribe(&panel, |_, _, _: &crate::files::panel::FilesEvent, _| {});
        self.sessions[0].files = Some(files::FilesTab {
            panel: panel.clone(),
            _subscription: subscription,
        });
        self.sessions[0].panel = Panel::Files;
        if scene == "editor" {
            let path = "/var/www/app/nginx.conf".to_string();
            let file = shellmate_sftp::TextFile {
                path: path.clone(),
                target: path,
                text: DATA["FILEBODY"]["nginx.conf"].s().into(),
                revision: shellmate_sftp::Revision {
                    size: 2316,
                    mtime: Some(proto::timestamp("11-12 08:55")),
                },
                line_ending: shellmate_sftp::LineEnding::Lf,
                bom: false,
                mode: Some(0o644),
            };
            let editor = cx.new(|cx| FileEditor::new(file, "prod-web-01", panel.downgrade(), window, cx));
            FileEditor::open(editor.clone(), window, cx);
            self.editor = Some(editor);
        }
    }
    pub(super) fn proto_reconnect_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let scene = std::env::var("SHELLMATE_DEMO").ok()?;
        if scene != "proto:reconnect" {
            return None;
        }
        let p = cx.palette().clone();
        let attempt = proto::reading("reconnect", cx.is_dark()).reconnect;
        Some(div().absolute().inset_0().rounded(CARD_RADIUS).bg(gpui::rgba(0x0a0c1080)).flex().items_center().justify_center()
        .child(div().w(px(428.)).h(px(229.)).p(px(24.)).rounded(px(22.)).bg(p.card).shadow(crate::ui::card_shadow(&p)).flex().flex_col().items_center().gap(px(14.))
            .child(div().size(px(34.)).relative().rounded_full().border_2().border_color(p.accs).child(gpui::canvas(|_,_,_|(),move|bounds,_,window,_|{
                let mut arc=gpui::PathBuilder::stroke(px(2.));let center=bounds.center();
                for i in 0..20{let a=-std::f32::consts::FRAC_PI_2+i as f32*std::f32::consts::PI/28.;let point=gpui::point(center.x+px(a.cos()*16.),center.y+px(a.sin()*16.));if i==0{arc.move_to(point);}else{arc.line_to(point);}}
                if let Ok(path)=arc.build(){window.paint_path(path,p.acc);}
            }).absolute().inset_0()))
            .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).child("网络已切换，正在重连"))
            .child(div().text_center().text_size(px(13.)).line_height(px(21.)).text_color(p.tx2).child(format!("Wi-Fi「Office-5G」→ 有线网络 · 第 {attempt} / 5 次尝试\n恢复后自动还原标签、工作目录与端口转发")))
            .child(div().flex().gap(px(8.)).child(Button::new("proto-retry").primary().w(px(88.)).h(px(36.)).rounded(px(11.)).text_size(px(13.)).label("立即重连")).child(Button::new("proto-close").w(px(88.)).h(px(36.)).rounded(px(11.)).text_size(px(13.)).label("关闭标签"))))
        .into_any_element())
    }
    fn proto_hostkey(&mut self, scene: &str, window: &mut Window, cx: &mut Context<Self>) {
        let changed = scene == "hostkey-changed";
        self.sidebar.update(cx, |s, cx| {
            s.select(Some(if changed { "h5" } else { "h9" }.into()), cx)
        });
        let (request, rx) = tokio::sync::oneshot::channel();
        drop(rx);
        let info = |value: &str| shellmate_ssh::HostKeyInfo {
            algorithm: "ssh-ed25519".into(),
            fingerprint: value.into(),
        };
        crate::dialogs::hostkey::HostKeyDialog::open(
            shellmate_ssh::HostKeyRequest {
                host: if changed { "staging-api" } else { "homelab-nas" }.into(),
                port: 22,
                state: if changed {
                    shellmate_core::FingerprintState::Changed
                } else {
                    shellmate_core::FingerprintState::New
                },
                key: info(if changed {
                    "SHA256:k9Fq1Zp0sW3yR7tBn2Mv6cXe8LjD4hUa5oGi0QwE1rT"
                } else {
                    "SHA256:7mJ2kQ9Lx0vT3bRk1nWc4ePZa8uYf6HsD5gVq0iN2oE"
                }),
                recorded: if changed {
                    vec![shellmate_ssh::HostKeyInfo {
                        algorithm: "ecdsa-sha2-nistp256".into(),
                        fingerprint: "SHA256:Xb4tN0q2mR8sVw1LcK7yHd3eJf9aPz5uQg6iO2nTr4E".into(),
                    }]
                } else {
                    vec![]
                },
            },
            request,
            window,
            cx,
        );
    }
}
