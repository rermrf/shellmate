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
        self.sessions.clear();
        self.active = None;
        self.store = Rc::new(RefCell::new(store));
        self.reload_book(window, cx);
        for host in self.book.hosts.iter().take(2).cloned().collect::<Vec<_>>() {
            let terminal = cx.new(|cx| Terminal::new(cx.terminal_style().palette.clone(), cx));
            let title = host.name.clone();
            terminal.update(cx, |t, cx| { t.attach(|sink: OutputSink, _| {
                sink.output(format!("\x1b[?2004hLast login: Thu Nov 12 09:41:03 2026 from 10.0.0.5\r\n\x1b[32mdeploy@{title}\x1b[0m:~$ cd /var/www/app\r\n\x1b[32mdeploy@{title}\x1b[0m:\x1b[34m/var/www/app\x1b[0m$ ls -la\r\ntotal 48\r\ndrwxr-xr-x  deploy www  4096 Nov 10 14:03 \x1b[34mconfig\x1b[0m\r\ndrwxr-xr-x  deploy www  4096 Nov 12 09:12 \x1b[34mdist\x1b[0m\r\n-rwxr-xr-x  deploy www   986 Nov 09 21:15 \x1b[32mdeploy.sh\x1b[0m\r\n-rw-r--r--  deploy www  1873 Nov 11 16:47 docker-compose.yml\r\n-rw-r--r--  deploy www  2316 Nov 12 08:55 nginx.conf\r\n\x1b[32mdeploy@{title}\x1b[0m:/var/www/app$ systemctl status nginx --no-pager\r\n\x1b[32m●\x1b[0m nginx.service - A high performance web server and a reverse proxy server\r\n     Loaded: loaded (/lib/systemd/system/nginx.service; enabled; preset: enabled)\r\n     Active: \x1b[32mactive (running)\x1b[0m since Wed 2026-11-11 22:03:17 CST; 11h ago\r\n   Main PID: 1184 (nginx)\r\n     Memory: 38.2M\r\n\x1b[32mdeploy@{title}\x1b[0m:\x1b[34m/var/www/app\x1b[0m$ ").as_bytes());
                Ok(Box::new(DemoBackend))
            }, cx); });
            let view = cx.new(|cx| {
                let mut v = TerminalView::new(terminal.clone(), cx.terminal_style(), cx);
                v.top_radius = CARD_RADIUS;
                v
            });
            let id = self.next_id;
            self.next_id += 1;
            let subscription = self.watch_terminal(id, &terminal, window, cx);
            let ssh = ssh::SshSession::demo(host.clone());
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
                Snippet {
                    name: "负载".into(),
                    command: "uptime".into(),
                },
                window,
                cx,
            ),
            "confirm-risk" => self.confirm_command(
                Snippet {
                    name: "清理悬空镜像".into(),
                    command: "docker image prune -f".into(),
                },
                window,
                cx,
            ),
            "history" => self.open_history_scope(false, window, cx),
            "history-host" => self.open_history_scope(true, window, cx),
            _ => log::warn!("unknown productivity demo scenario"),
        }
        cx.notify();
    }
}
