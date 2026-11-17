//! Debug-only, deterministic P6 screenshots. Fixtures never write to the supplied database.
use super::*;
use shellmate_core::{Group, GroupTone, Host, TerminalBackend};
use shellmate_term::OutputSink;

struct DemoBackend;
impl TerminalBackend for DemoBackend {
    fn write(&mut self, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn resize(&mut self, _: u16, _: u16, _: u16, _: u16) -> std::io::Result<()> {
        Ok(())
    }
    fn close(&mut self) {}
}

impl Workspace {
    pub(crate) fn demo_productivity(&mut self, scenario: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut store = HostStore::open_in_memory().expect("demo database");
        let production = Group::new("生产环境", GroupTone::Accent);
        let staging = Group::new("测试环境", GroupTone::Warn);
        store.save_group(&production).unwrap();
        store.save_group(&staging).unwrap();
        for (i, (name, addr)) in [
            ("prod-web-01", "10.0.1.21"),
            ("prod-db-01", "10.0.2.8"),
            ("prod-web-02", "10.0.1.22"),
            ("bastion", "bastion.corp.io"),
            ("staging-api", "172.16.4.10"),
        ]
        .into_iter()
        .enumerate()
        {
            let mut host = Host::new(name, addr);
            host.user = Some(if i == 1 { "dba" } else { "deploy" }.into());
            host.group = Some(if i < 4 {
                production.id.clone()
            } else {
                staging.id.clone()
            });
            host.last_connected_at = Some(100 - i as i64);
            host.favorite = i < 2;
            store.save_host(&host).unwrap();
        }
        // Existing startup sessions are closed; all demo terminal backends discard input.
        for id in self.sessions.iter().map(|s|s.id).collect::<Vec<_>>(){self.finalize_archives(id,cx);}
        self.sessions.clear();
        self.active = None;
        self.store = shellmate_store::SharedStore::new(store);
        self.secrets=crate::security::fixture_coordinator(self.store.clone());
        self.reload_book(window, cx);
        let count = if scenario == "tabs" {
            7
        } else if scenario.starts_with("broadcast-") || scenario == "split-menu" {
            3
        } else {
            2
        };
        let mut demo_hosts = self.book.hosts.iter().take(count).cloned().collect::<Vec<_>>();
        while demo_hosts.len() < count {
            demo_hosts.push(Host::new(format!("dev-box-{}", demo_hosts.len()), "demo.invalid"));
        }
        if scenario == "broadcast-confirm" {
            demo_hosts[1].group = Some(staging.id.clone());
            demo_hosts[1].name = "staging-api".into();
        }
        for host in demo_hosts {
            let terminal = cx.new(|cx| Terminal::new(cx.terminal_style().palette.clone(), cx));
            let title = host.name.clone();
            terminal.update(cx, |t, cx| { t.attach(|sink: OutputSink, _| {
                sink.output(format!("\x1b[?2004hLast login: Thu Nov 12 09:41:03 2026 from 10.0.0.5\r\n\x1b[32mdeploy@{title}\x1b[0m:~$ cd /var/www/app\r\n\x1b[32mdeploy@{title}\x1b[0m:\x1b[34m/var/www/app\x1b[0m$ ls -la\r\ntotal 48\r\ndrwxr-xr-x  deploy www  4096 Nov 10 14:03 \x1b[34mconfig\x1b[0m\r\ndrwxr-xr-x  deploy www  4096 Nov 12 09:12 \x1b[34mdist\x1b[0m\r\n-rwxr-xr-x  deploy www   986 Nov 09 21:15 \x1b[32mdeploy.sh\x1b[0m\r\n-rw-r--r--  deploy www  1873 Nov 11 16:47 docker-compose.yml\r\n-rw-r--r--  deploy www  2316 Nov 12 08:55 nginx.conf\r\n\x1b[32mdeploy@{title}\x1b[0m:/var/www/app$ systemctl status nginx --no-pager\r\n\x1b[32m●\x1b[0m nginx.service - A high performance web server and a reverse proxy server\r\n     Loaded: loaded (/lib/systemd/system/nginx.service; enabled; preset: enabled)\r\n     Active: \x1b[32mactive (running)\x1b[0m since Wed 2026-11-11 22:03:17 CST; 11h ago\r\n   Main PID: 1184 (nginx)\r\n     Memory: 38.2M\r\n\x1b[32mdeploy@{title}\x1b[0m:\x1b[34m/var/www/app\x1b[0m$ ").as_bytes());
                Ok(Box::new(DemoBackend))
            }, cx); });
            let view = cx.new(|cx| {
                let mut v = TerminalView::new(terminal.clone(), cx.terminal_style(), cx).with_search_icon(crate::icons::AppIcon::Search.at(14.));
                v.top_radius = CARD_RADIUS;
                v
            });
            let id = self.next_id;
            self.next_id += 1;
            let subscription = self.watch_terminal(id, &terminal, window, cx);
            let ssh = ssh::SshSession::demo(host.clone());
            self.sessions.push(Session {
                ai_alive: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
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
        let host_ids: Vec<_> = self.book.hosts.iter().take(2).map(|h| h.id.to_string()).collect();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for (i, (command, host, label)) in [
            (
                "systemctl status nginx --no-pager",
                host_ids[0].as_str(),
                "prod-web-01",
            ),
            ("ls -la", host_ids[0].as_str(), "prod-web-01"),
            ("cd /var/www/app", host_ids[0].as_str(), "prod-web-01"),
            ("ssh prod-web-01", "local", "本机"),
            ("git status", "local", "本机"),
            ("mysql -u root -p shop", host_ids[1].as_str(), "prod-db-01"),
            (
                "docker compose logs -f --tail 100 app",
                host_ids[0].as_str(),
                "prod-web-01",
            ),
            (
                "tail -f /var/log/nginx/access.log",
                host_ids[0].as_str(),
                "prod-web-01",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            self.store
                .borrow_mut()
                .add_history(&HistoryEntry {
                    id: 0,
                    command: command.into(),
                    host: host.into(),
                    label: label.into(),
                    cwd: Some("/var/www/app".into()),
                    time: now - i as i64 * 120,
                })
                .unwrap();
        }
        if scenario != "commands-empty" {
            self.store.borrow_mut().save_snippets(&model::samples()).unwrap();
        }
        self.refresh_snippets(cx);
        self.activate(0, window, cx);
        self.commands_open = matches!(
            scenario,
            "commands" | "commands-empty" | "manage" | "confirm" | "confirm-risk"
        );
        match scenario {
            "palette" => self.open_palette_query("", window, cx),
            "palette-prod" => self.open_palette_query("prod", window, cx),
            "palette-connect" => self.open_palette_query("deploy@new.example.com:2222", window, cx),
            "commands" | "commands-empty" => {}
            "manage" => Manager::open(
                model::samples(),
                self.store.clone(),
                cx.entity().downgrade(),
                window,
                cx,
            ),
            "confirm" => self.confirm_command(
                Snippet { id: String::new(),
                    name: "负载".into(),
                    command: "uptime".into(),
                },
                window,
                cx,
            ),
            "confirm-risk" => self.confirm_command(
                Snippet { id: String::new(),
                    name: "清理悬空镜像".into(),
                    command: "docker image prune -f".into(),
                },
                window,
                cx,
            ),
            "history" => self.open_history_scope(false, window, cx),
            "history-host" => self.open_history_scope(true, window, cx),
            "broadcast-off" => {
                self.commands_open = true;
            }
            "broadcast-on" | "broadcast-suggestions" | "broadcast-confirm" => {
                self.broadcast.checked = self.sessions.iter().map(|s| s.id).collect();
                if let SessionKind::Ssh(ssh) = &mut self.sessions[2].kind {
                    ssh.status = ssh::SshStatus::Reconnecting(2);
                }
                self.set_broadcast(true, window, cx);
                if scenario == "broadcast-suggestions" {
                    self.broadcast_bar.update(cx, |b, cx| {
                        b.set_text("docker", window, cx);
                        b.focus(window, cx);
                    });
                }
                if scenario == "broadcast-confirm" {
                    self.submit_broadcast("docker image prune -f", window, cx);
                }
            }
            "split-right" | "split-vertical" | "split-menu" => {
                self.toggle_split(window, cx);
                self.focus_pane(crate::productivity::multi::Pane::Other, window, cx);
                if scenario == "split-vertical" {
                    self.flip_split(window, cx);
                }
                if scenario == "split-menu" {
                    self.open_session_menu(
                        crate::productivity::session_menu::Kind::Split,
                        gpui::point(window.viewport_size().width * 0.6, px(148.)),
                        window,
                        cx,
                    );
                }
            }
            "tabs" => {
                for (ix, status) in [
                    (4, ssh::SshStatus::Reconnecting(2)),
                    (5, ssh::SshStatus::Failed),
                    (6, ssh::SshStatus::Ended),
                ] {
                    if let SessionKind::Ssh(ssh) = &mut self.sessions[ix].kind {
                        ssh.status = status;
                    }
                }
                self.open_session_menu(
                    crate::productivity::session_menu::Kind::Tabs,
                    gpui::point(window.viewport_size().width - px(500.), px(50.)),
                    window,
                    cx,
                );
            }
            _ => log::warn!("unknown productivity demo scenario"),
        }
        self.sync_productivity(cx);
        cx.notify();
    }
}
