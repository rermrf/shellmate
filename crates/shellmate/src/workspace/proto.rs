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
        let (scene,_) = proto::request(scene);
        if !proto::SCENES.contains(&scene) && !proto::monitor::PORT_SCENES.contains(&scene) && !proto::monitor::SERVICE_SCENES.contains(&scene) {
            log::warn!("unknown prototype scene");
            return;
        }
        let scrolled = proto::PAGE_SCROLL_SCENES.iter().any(|(name, _)| *name == scene);
        let scene = if scrolled { scene.strip_suffix("-scrolled").unwrap_or(scene) } else { scene };
        let stress=scene.starts_with("stress-");
        let scene=scene.strip_prefix("stress-").unwrap_or(scene);
        self.ai_drawer=None;self.ai_open=false;
        self.commands_open = false;
        self.drawer_motion = Default::default();
        self.ai_drawer_width = if scene == "ai-markdown-wide" { 560. } else { drawer_resize::DEFAULT_WIDTH };
        self.sessions.clear();
        self.hub.groups.borrow_mut().clear();
        self.active = None;
        self.monitor_frozen = true;
        *self.hub.preferences.borrow_mut() = Default::default();
        for (i, (name, tone)) in [
            ("生产环境", GroupTone::Accent),
            ("测试环境", GroupTone::Warn),
            ("个人", GroupTone::Ok),
            ("设备", GroupTone::Neutral),
        ]
        .into_iter()
        .enumerate()
        {
            self.hub.store
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
        let mut hosts=hosts;
        if scrolled && scene == "side" {
            // One deterministic section: selected row 4, 50px rows with 2px gaps.
            hosts = (0..20).map(|i| {
                let mut host = hosts[0].clone();
                host.id = HostId::from_raw(match i {
                    0 => "h1".to_string(), 1 => "h3".to_string(),
                    2 => "h10".to_string(), 3 => "h7".to_string(),
                    _ => format!("scroll-host-{i:02}"),
                });
                host.name = format!("prod-web-{i:02}");
                host.addr = format!("10.0.0.{}", i + 1);
                host.group = Some(GroupId::from_raw("生产环境"));
                host.favorite = false;
                host.last_connected_at = None;
                host
            }).collect();
        }
        if stress {
            for (i,host) in hosts.iter_mut().enumerate() {
                host.name=format!("{}-{i}",proto::stress::HOST);host.addr=host.name.clone();host.user=Some(proto::stress::USER.into());host.port=65535;host.protocol=Protocol::Ssh;
            }
            let mut group=self.hub.store.borrow().groups().unwrap().remove(0);
            group.name="生产环境 · 欧洲西部数据中心 · 客户数据与结算平台".into();self.hub.store.borrow_mut().save_group(&group).unwrap();
        }
        self.hub.store.borrow_mut().save_hosts(&hosts).unwrap();
        self.reload_book(window, cx);
        self.hub.store
            .borrow_mut()
            .save_snippets(&crate::productivity::model::samples())
            .unwrap();
        self.refresh_snippets(cx);
        for h in DATA["HIST"].a().iter().rev() {
            let label = h["h"].s();
            let host = self.hub
                .book.borrow()
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
            self.hub.store
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

        let mut session_sources=vec![("h1", "S1"), ("h3", "S2"), ("h10", "S3"), ("h7", "S4")];
        if scene=="telnet" {session_sources.push(("h11","TELNET"));}
        if scene=="mosh" {session_sources.push(("h8","MOSH"));}
        if stress { session_sources.extend([("h2","S1"),("h4","S1"),("h5","S1"),("h6","S1"),("h8","S1")]); }
        for (i, (host_id, lines)) in session_sources.into_iter().enumerate() {
            let host = self.hub.book.borrow().host(&HostId::from_raw(host_id)).unwrap().clone();
            let terminal = cx.new(|cx| Terminal::new(cx.terminal_style().palette, cx));
            terminal.update(cx, |t, cx| {
                let mut size = t.size();
                size.cols = 160;
                size.rows = 40;
                t.set_size(size);
                if scene == "prompt-plain" && i == 0 {
                    let mut output = None;
                    t.attach(|sink, _| {
                        output = Some(sink);
                        Ok(Box::new(Inert))
                    }, cx);
                    let sink = output.expect("fixture backend attached");
                    cx.spawn(async move |_, cx| {
                        cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                        for text in proto::prompt_plain::STEPS {
                            sink.output(text.as_bytes());
                            cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
                        }
                    }).detach();
                    return;
                }
                let text = if lines=="MOSH" {"\x1b[32;1mme@dev-box\x1b[0m:\x1b[34;1m~\x1b[0m$ echo mosh-$((6*7))\r\nmosh-42\r\n\x1b[32;1mme@dev-box\x1b[0m:\x1b[34;1m~\x1b[0m$ \x1b[?25l".into()}else if lines=="TELNET" {
                    "\x1b[90mTrying 192.168.1.1...\x1b[0m\r\nConnected to 192.168.1.1. Escape character is ^].\r\nRT-AX86U login: admin\r\nPassword:\r\n\x1b[32;1madmin@RT-AX86U\x1b[0m:\x1b[34;1m~\x1b[0m$ ".to_string()
                }else if host.protocol==Protocol::Serial {
                    let mut display=shellmate_serial::transform::Display::default();display.options(scene=="serial-hex",false);
                    let raw=if scene=="serial-hex" {DATA[lines].a().iter().map(|line|line.a().iter().map(|s|s["t"].s()).collect::<String>()).collect::<Vec<_>>().join("\r\n")}else{proto::ansi(&DATA[lines],dark)};
                    let mut bytes=display.feed(raw.as_bytes(),"09:41:03.000");bytes.extend(display.flush("09:41:03.000"));if scene=="serial-hex" {format!("\x1b[36m{}\x1b[0m\x1b[?25l",String::from_utf8(bytes).unwrap())}else{String::from_utf8(bytes).unwrap()}
                }else{proto::ansi(&DATA[lines], dark)};
                if scene == "split" || scene=="serial-hex" {
                    // Let both real pane geometries settle before feeding mock output. Shrinking
                    // a populated grid reflows and scrolls it, hiding the beginning of the fixture.
                    cx.spawn(async move |terminal,cx| {
                        cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                        terminal.update(cx,|t,cx|{t.attach(move |sink:OutputSink,_| {
                            sink.output(text.as_bytes());Ok(Box::new(Inert))
                        },cx);t.scroll_to_edge(true);}).ok();
                    }).detach();
                } else {
                    t.attach(move |sink:OutputSink,_| {sink.output(text.as_bytes());Ok(Box::new(Inert))},cx);
                }
            });
            let view = cx.new(|cx| {
                let mut v = TerminalView::new(terminal.clone(), cx.terminal_style(), cx).with_search_icon(crate::icons::AppIcon::Search.at(14.));
                v.top_radius = CARD_RADIUS;
                v
            });
            let id = self.hub.session_ids.allocate();
            let subscription = self.watch_terminal(id, &terminal, window, cx);
            let mut ssh = ssh::SshSession::demo(host.clone());
            if let Some(mosh)=&mut ssh.mosh {mosh.independent=true;mosh.port=Some(60001);}
            if i == 3 {
                ssh.status = ssh::SshStatus::Reconnecting(proto::reading(scene, dark).reconnect);
            }
            let kind=if matches!(host.protocol,Protocol::Serial|Protocol::Telnet) {
                let mut stream=streams::StreamSession::new(host.clone());stream.connected=true;
                if let Some(state)=&mut stream.snapshot {state.open=true;state.received=18637;state.hex=scene=="serial-hex";}
                SessionKind::Stream(Box::new(stream))
            }else{SessionKind::Ssh(Box::new(ssh))};
            self.sessions.push(Session {
            ai_alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                id,
                title: host.name.into(),
                view,
                terminal,
                exited: false,
                panel: Panel::Terminal,
                files: None,
                forward: None,
                monitor: None,
                typed: None, typing: None,
                kind,
                _subscription: Some(subscription),
            });
        }

        self.activate(if matches!(scene,"serial"|"serial-hex"){2}else if matches!(scene,"telnet"|"mosh"){4}else{0}, window, cx);
        self.broadcast.checked = [1, 2].into();
        self.sync_productivity(cx);
        let settings = |section, window: &mut Window, cx: &mut Context<Self>| {
            crate::settings::SettingsWindow::open(cx.entity().downgrade(), self.hub.preferences.borrow().clone(),
                self.hub.secrets.clone(), section, self.hub.store.clone(), window, cx)
        };
        match scene {
            "close-window-dialog" | "tabmenu-move" => {
                self.demo_multi_window(scene == "close-window-dialog", window, cx);
            }
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
            "drawer-mid" | "drawer-swap-mid" => {
                use drawer_motion::Drawer;
                self.open_ai(window, cx);
                if let Some(drawer) = &self.ai_drawer {
                    drawer.update(cx, |drawer, cx| drawer.scene("ai", cx));
                }
                if scene == "drawer-swap-mid" {
                    self.toggle_commands(window, cx);
                    self.drawer_motion.freeze(Drawer::Ai, Drawer::Commands);
                } else {
                    self.drawer_motion.freeze(Drawer::Closed, Drawer::Ai);
                }
            }
            "rail-mid" | "rail-mid-expand" => {
                let open = scene == "rail-mid-expand";
                self.set_sidebar_open(open, cx);
                self.sidebar_motion = SidebarMotion::frozen_midpoint(open);
            }
            "local" => self.proto_local(window, cx),
            "files-local" | "files-local-trash" => self.proto_local_files(scene == "files-local-trash", window, cx),
            "reconnect" => self.activate(3, window, cx),
            "files" | "editor" => self.proto_files(if stress {"stress-files"}else if scrolled {"files-scrolled"}else{scene}, window, cx),
            "fwd" | "fwd-new" => {
                let mut view = if stress {proto::stress::forward()}else{crate::forward::demo::view("cards", "prod-web-01")};
                if scrolled {
                    let samples = view.cards.clone();
                    view.cards = (0..12).map(|i| {
                        let mut card = samples[i % samples.len()].clone();
                        card.id = shellmate_forward::RuleId::from_raw(format!("scroll-rule-{i}"));
                        card.name = format!("滚动示例规则 {}", i + 1);
                        card
                    }).collect();
                }
                let panel = cx.new(|cx| crate::forward::panel::ForwardPanel::new(view, cx));
                let subscription =
                    cx.subscribe(&panel, |_, _, _: &crate::forward::panel::ForwardPanelEvent, _| {});
                if scene == "fwd-new" {
                    panel.update(cx, |p, cx| p.toggle_form(window, cx));
                }
                self.sessions[0].forward = Some(forward::ForwardTab {
                    panel,
                    _subscription: Some(subscription),
                });
                self.sessions[0].panel = Panel::Forward;
            }
            "mon-ov" | "mon-proc" | "mon-svc" | "mon-dk" | "mon-ov-reflow-mid" => self.proto_monitor(scene, stress, window, cx),
            local if matches!(local, "mon-local" | "mon-local-proc" | "mon-local-disks") || proto::monitor::PORT_SCENES.contains(&local) || proto::monitor::SERVICE_SCENES.contains(&local) => {
                self.proto_local(window, cx);
                self.proto_monitor(scene, false, window, cx);
            }
            "rv" | "rv-log" | "rv-end" | "rv-wide" | "rv-recording" => {
                let panel = crate::records::panel::RecordsWindow::open(
                    self.hub.store.clone(),
                    PathBuf::from("/nonexistent-shellmate-fixture"),
                    None,
                    false,
                    Default::default(),
                    window,
                    cx,
                );
                if let Some(view) = panel.panel() {
                    view.update(cx, |r, cx| r.proto_scene(scene, cx));
                }
                *self.hub.records_window.borrow_mut() = Some(panel);
            }
            "host-new" | "host-new-focus" | "host-serial" | "host-telnet" | "host-mosh" => self.new_host(window, cx),
            "hostkey-new" | "hostkey-changed" => self.proto_hostkey(scene, window, cx),
            "ai-agent" | "ai-agent-run" | "ai-new-confirm" | "ai-agent-redline"
            | "ai-agent-full" | "ai" | "ai-chat" | "ai-markdown" | "ai-markdown-wide"
            | "ai-history" | "ai-error" | "ai-unconfigured" => {
                self.open_ai(window, cx);
                if let Some(drawer) = &self.ai_drawer {
                    drawer.update(cx, |d, cx| {
                        d.scene(if scene == "ai-markdown-wide" { "ai-markdown" } else { scene }, cx);
                        if scene == "ai-agent-full" {
                            d.confirm_full(window, cx);
                        } else if scene == "ai-new-confirm" {
                            d.new_conversation(window, cx);
                        }
                    });
                }
            }
            "ai-explain"=>{self.sessions[0].terminal.update(cx,|t,cx|t.notice("[error] connect() failed: Connection refused",cx));},
            s if s.starts_with("update-") || matches!(s, "settings-update" | "main-update-pill") => {
                crate::updater::scene(s, cx);
                if s == "settings-update" { self.open_settings(6, window, cx); }
                if s.starts_with("update-") { crate::updater::open(window, cx); }
            }
            "set-ai" | "set-ai-replace" => {
                self.hub.store.borrow_mut().save_ai_settings(&shellmate_store::ai::Settings{model:"gpt-5".into(),reasoning:shellmate_ai::ReasoningLevel::Medium,..Default::default()}).unwrap();
                let view = settings(5, window, cx);
                if scene == "set-ai-replace" { view.update(cx, |settings, cx| settings.fixture_replace_key(cx)); }
            }
            "set-ai-anthropic"=>{self.hub.store.borrow_mut().save_ai_settings(&shellmate_store::ai::Settings{provider:shellmate_ai::Provider::Anthropic{base_url:"https://anthropic-proxy.example.com/v1".into()},model:"claude-sonnet-4-6".into(),reasoning:shellmate_ai::ReasoningLevel::High,..Default::default()}).unwrap();self.open_settings(5,window,cx);},
            "set-cred" => self.open_settings(0, window, cx),
            "set-cred-master" | "set-cred-focus" | "set-cred-typed" => {
                let (value, focused) = match scene {
                    "set-cred-focus" => ("", true),
                    "set-cred-typed" => ("demo", true),
                    _ => ("meadow river stone !", false),
                };
                settings(0, window, cx).update(cx, |settings, cx| settings.fixture_master(value, focused, window, cx));
            }
            "vault-unlock" => {self.hub.secrets.switch_to_master("Fixture9!Meadow",shellmate_vault::KdfParams{memory_kib:8192,iterations:1,parallelism:1}).expect("fixture vault");self.hub.secrets.set_touch_id(true).expect("fixture device key");self.hub.secrets.lock();crate::security::PasswordDialog::open(self.hub.secrets.clone(),crate::security::Purpose::Unlock("连接 prod-web-01".into()),None,Box::new(|_,_,_|{}),window,cx);},
            "bundle-export" => {crate::security::PasswordDialog::open(self.hub.secrets.clone(),crate::security::Purpose::Export,None,Box::new(|_,_,_|{}),window,cx);},
            "bundle-preview" => {let mut incoming=self.hub.secrets.export_bundle(shellmate_store::bundle::Contents{credentials:false,..Default::default()}).expect("fixture bundle");if let Some(host)=incoming.items.iter_mut().find(|i|i.kind==shellmate_store::bundle::Kind::Host){host.fields.insert("addr".into(),serde_json::json!("10.0.1.99"));}
                if let Some(mut host)=incoming.items.iter().find(|i|i.kind==shellmate_store::bundle::Kind::Host).cloned(){host.id="fixture-import-new".into();host.fields.insert("name".into(),serde_json::json!("新设备"));incoming.items.push(host);}let preview=self.hub.secrets.preview_bundle(incoming).expect("fixture preview");crate::security::PreviewDialog::open_with_manager(preview,self.hub.secrets.clone(),None,window,cx);},
            "set-sync" => self.open_settings(1, window, cx),
            "set-term" => self.open_settings(2, window, cx),
            "set-term-scrolled" => {
                settings(2, window, cx).update(cx, |s, cx| s.fixture_scroll(120., cx));
            }
            "set-local" => self.open_settings(3, window, cx),
            "quit-confirm" => self.confirm_quit(super::lifecycle::Activity { sessions: 3, transfers: 1, forwards: 2 }, window, cx),
            "set-conn" | "settings-window" => self.open_settings(4, window, cx),
            _ => {}
        }
        self.sync_sidebar(cx);
        if scrolled {
            match scene {
                "mon-ov" | "mon-dk" | "mon-svc" | "mon-proc" => {
                    if scene == "mon-ov" { self.commands_open = true; }
                    if let Some(tab) = &self.sessions[0].monitor {
                        tab.panel.update(cx, |p, cx| p.fixture_scroll(window, cx));
                    }
                }
                "fwd" => {
                    let handle = self.sessions[0].forward.as_ref().unwrap().panel.read(cx).scroll.clone();
                    proto::scroll_after_layout(handle, |_| 120., window);
                }
                "side" => {
                    self.sidebar.update(cx, |s, cx| s.select(Some("scroll-host-04".into()), cx));
                    let handle = self.sidebar.read(cx).scroll.clone();
                    // Existing top inset 11 + section header 25 + gap 2 + 4 rows + half row.
                    proto::scroll_after_layout(handle, |_| 11. + 25. + 2. + 4. * 52. + 25., window);
                }
                "cmds" => {
                    self.commands.update(cx, |drawer, cx| {
                        let samples = drawer.groups.clone();
                        drawer.groups = (0..6).flat_map(|i| samples.iter().cloned().map(move |mut group| {
                            group.name = format!("{} · {}", group.name, i + 1);
                            for command in &mut group.items { command.id = format!("scroll-{i}-{}", command.id); }
                            group
                        })).collect();
                        cx.notify();
                    });
                    proto::scroll_after_layout(self.commands.read(cx).scroll.clone(), |_| 120., window);
                }
                "ai" => {
                    if let Some(drawer) = &self.ai_drawer {
                        drawer.update(cx, |d, cx| d.fixture_scroll(window, cx));
                    }
                }
                _ => {}
            }
        }
        cx.notify();
    }
    fn proto_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let request = self.send_request(vec![1], false, false);
        self.open_confirmation(
            shellmate_store::Snippet { id: String::new(),
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
        let view = cx.new(|cx| TerminalView::new(terminal.clone(), cx.terminal_style(), cx).with_search_icon(crate::icons::AppIcon::Search.at(14.)));
        let id = self.hub.session_ids.allocate();
        let subscription = self.watch_terminal(id, &terminal, window, cx);
        self.sessions.push(Session {
            ai_alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            id,
            title: "本机".into(),
            view,
            terminal,
            exited: false,
            panel: Panel::Terminal,
            files: None,
            forward: None,
            monitor: None,
            typed: None, typing: None,
            kind: SessionKind::Local,
            _subscription: Some(subscription),
        });

        *self.hub.user_host.borrow_mut() = "chen@MacBook-Pro".into();
        self.activate(4, window, cx);
    }

    fn proto_local_files(&mut self, trash: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = std::env::var_os("SHELLMATE_DATA_DIR") else { log::error!("files-local requires SHELLMATE_DATA_DIR"); return };
        self.proto_local(window, cx);
        let make = cx.background_spawn(async move { crate::files::local_fixture::prepare(std::path::Path::new(&data)) });
        cx.spawn_in(window, async move |this, cx| {
            if let Err(err) = make.await { log::error!("files-local: {err}"); return; }
            this.update_in(cx, |this, window, cx| {
                this.show_panel(Panel::Files, window, cx);
                if trash && let Some(files) = this.active.and_then(|ix| this.sessions[ix].files.as_ref()) {
                    files.panel.update(cx, |panel, _| panel.fixture_trash_pair());
                }
            }).ok();
        }).detach();
    }
}
impl Workspace {
    fn proto_monitor(&mut self, scene: &str, stress: bool, window: &mut Window, cx: &mut Context<Self>) {
        let ports = proto::monitor::PORT_SCENES.contains(&scene);
        let services = proto::monitor::SERVICE_SCENES.contains(&scene);
        let local = services || ports || matches!(scene, "mon-local" | "mon-local-proc" | "mon-local-disks");
        let ix = if local { self.active.expect("inert local fixture") } else { 0 };
        let data = if services { proto::monitor::local_services(scene) } else if ports { proto::monitor::local_ports(scene) }
            else if scene == "mon-local-disks" { proto::monitor::local_disks() } else if local { proto::monitor::local() }
            else if stress { proto::stress::monitor(scene, cx.is_dark()) } else { proto::monitor::data(scene, cx.is_dark()) };
        let panel =
            cx.new(|cx| crate::monitor::panel::MonitorPanel::new(if local { "本机" } else if stress{proto::stress::HOST}else{"prod-web-01"}, data, None, window, cx));
        let page = match scene {
            _ if ports => crate::monitor::model::Page::Ports,
            _ if services => crate::monitor::model::Page::Services,
            "mon-proc" | "mon-local-proc" => crate::monitor::model::Page::Processes,
            "mon-svc" => crate::monitor::model::Page::Services,
            "mon-dk" => crate::monitor::model::Page::Docker,
            _ => crate::monitor::model::Page::Overview,
        };
        panel.update(cx, |p, cx| {
            p.set_page(page, cx);
            p.frozen_reflow = scene == "mon-ov-reflow-mid";
            if scene == "mon-local-svc-sel" {
                p.demo_services("", crate::monitor::services::ServiceFilter::All, Some("gui/501/homebrew.mxcl.redis"), window, cx);
            }
            if scene == "mon-local-ports-sel" { p.demo_select_port(3000); }
            if matches!(scene, "mon-proc" | "mon-local-proc") {
                p.show_process(if stress{4_194_304}else{2207}, window, cx);
            }
            if scene == "mon-svc" {
                p.demo_services(
                    "",
                    crate::monitor::services::ServiceFilter::All,
                    Some(if stress{proto::stress::SERVICE}else{"php8.2-fpm.service"}),
                    window,
                    cx,
                );
                p.demo_service_log(crate::monitor::demo::service_log(), cx);
                p.proto_order(cx);
            }
            if scene == "mon-dk" {
                p.demo_container_log(if stress{proto::stress::CONTAINER}else{"worker"}, crate::monitor::demo::container_log(), cx);
            }
        });
        let subscription = cx.subscribe(&panel, |_, _, _: &crate::monitor::panel::MonitorPanelEvent, _| {});
        self.sessions[ix].monitor = Some(monitor::MonitorTab {
            panel,
            page,
            _subscription: Some(subscription),
        });
        self.sessions[ix].panel = Panel::Monitor;
    }
    fn proto_files(&mut self, scene: &str, window: &mut Window, cx: &mut Context<Self>) {
        let terminal = self.sessions[0].terminal.clone();
        let Some(runtime) = crate::runtime::get(cx) else {
            return;
        };
        let panel = cx.new(|cx| crate::files::panel::FilesPanel::new(&terminal, runtime, window, cx));
        panel.update(cx, |p, cx| {
            p.proto_fixture(scene, cx);
            if scene == "files-scrolled" { p.fixture_scroll(window); }
        });
        let subscription = cx.subscribe(&panel, |_, _, _: &crate::files::panel::FilesEvent, _| {});
        self.sessions[0].files = Some(files::FilesTab {
            panel: panel.clone(),
            _subscription: Some(subscription),
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
            self.open_editor(self.sessions[0].id, file, window, cx);
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
            .child(div().text_size(px(17.)).line_height(px(24.)).font_weight(FontWeight::SEMIBOLD).child("网络已切换，正在重连"))
            .child(div().text_center().text_size(px(13.)).line_height(px(21.)).text_color(p.tx2).child(format!("Wi-Fi「Office-5G」→ 有线网络 · 第 {attempt} / 5 次尝试\n恢复后自动还原会话、工作目录与端口转发")))
            .child(div().flex().gap(px(8.)).mt(px(4.)).child(Button::new("proto-retry").primary().w(px(88.)).h(px(36.)).rounded(px(11.)).text_size(px(13.)).label("立即重连")).child(Button::new("proto-close").w(px(88.)).h(px(36.)).rounded(px(11.)).text_size(px(13.)).label("关闭会话"))))
        .into_any_element())
    }
    fn proto_hostkey(&mut self, scene: &str, window: &mut Window, cx: &mut Context<Self>) {
        let changed = scene == "hostkey-changed";
        self.sidebar.update(cx, |s, cx| {
            s.select(Some(if changed { "h5" } else { "h9" }.into()), cx)
        });
        let Some(session) = self.main_id() else { return };
        let (reply, answer) = tokio::sync::oneshot::channel();
        cx.spawn(async move |_, _| {
            if let Ok(decision) = answer.await {
                log::info!("prototype host key decision: {decision:?}");
            }
        }).detach();
        let info = |value: &str| shellmate_ssh::HostKeyInfo {
            algorithm: "ssh-ed25519".into(),
            fingerprint: value.into(),
        };
        let request = shellmate_ssh::HostKeyRequest {
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
        };
        self.open_connection_prompt(session, connect::UiMsg::HostKey { request, reply }, window, cx);
    }
}
