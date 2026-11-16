//! Debug-only review hooks: `SHELLMATE_DEMO=<name>` opens one dialog on startup, filled with
//! fake data, so every dialog can be looked at without a server (and `SHELLMATE_THEME=dark|light`
//! forces the theme). Compiled out of release builds.

use gpui::{App, Entity, Window};
use shellmate_core::FingerprintState;
use shellmate_ssh::{
    ChallengePrompt, ChallengeRequest, HostKeyInfo, HostKeyRequest, PassphraseRequest, PasswordRequest,
};
use tokio::sync::oneshot;

use crate::dialogs::auth::{ChallengeDialog, SecretDialog};
use crate::dialogs::confirm::ConfirmDialog;
use crate::dialogs::host_editor::{EditorMode, HostEditor};
use crate::dialogs::hostkey::HostKeyDialog;
use crate::dialogs::import::{ImportDialog, ImportNotes};
use crate::workspace::Workspace;

/// Names accepted by `SHELLMATE_DEMO` (`connect:<host name>` also connects a saved host on startup; `files:<host name>[:<dir>[:<name>]]` and
/// `files-hint:<host name>` connect one and show its files panel;
/// `connect-dup:<host name>` does that and copies the session a few seconds later, like ⌘⇧T).
pub const NAMES: &[&str] = &[
    "hostkey-new",
    "hostkey-changed",
    "password",
    "passphrase",
    "otp",
    "kbd",
    "host-editor",
    "host-editor-edit",
    "import",
    "delete",
    "p6:palette", "p6:palette-prod", "p6:palette-connect", "p6:commands", "p6:commands-empty",
    "p6:manage", "p6:confirm", "p6:confirm-risk", "p6:history", "p6:history-host",
    "p6:broadcast-off", "p6:broadcast-on", "p6:broadcast-suggestions", "p6:broadcast-confirm",
    "p6:split-right", "p6:split-vertical", "p6:split-menu", "p6:tabs",
    "p6:records", "p6:records-delete", "p6:logs", "p6:recording", "p6:settings-cred", "p6:settings-sync", "p6:settings-term", "p6:settings-local", "p6:settings-conn",
];

fn key(algorithm: &str, fingerprint: &str) -> HostKeyInfo {
    HostKeyInfo { algorithm: algorithm.into(), fingerprint: fingerprint.into() }
}

fn challenge(round: u32, attempt: u32, prompts: &[(&str, bool)]) -> ChallengeRequest {
    ChallengeRequest {
        host: "bastion.corp.io".into(),
        port: 2222,
        user: "ops".into(),
        name: String::new(),
        instructions: String::new(),
        prompts: prompts.iter().map(|(text, echo)| ChallengePrompt { text: text.to_string(), echo: *echo }).collect(),
        round,
        attempt,
    }
}

/// Opens the demo named `name`, or logs the valid names.
pub fn run(name: &str, workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
    if let Some(scene)=name.strip_prefix("proto:"){workspace.update(cx,|w,cx|w.proto_scene(scene,window,cx));return;}
    if let Some(args) = name.strip_prefix("p6live:") {
        if args.starts_with("record:")||args.starts_with("log:"){workspace.update(cx,|w,cx|w.demo_archive_live(args,window,cx));return;}
        workspace.update(cx, |workspace,cx|workspace.demo_productivity_live(args,window,cx));
        return;
    }
    if let Some(scenario) = name.strip_prefix("p6:") {
        if matches!(scenario,"records"|"records-delete"|"logs"|"recording")||scenario.starts_with("settings-"){workspace.update(cx,|w,cx|w.demo_archives(scenario,window,cx));return;}
        workspace.update(cx, |workspace, cx| workspace.demo_productivity(scenario, window, cx));
        return;
    }
    if let Some(host) = name.strip_prefix("connect:") {
        workspace.update(cx, |workspace, cx| workspace.connect_by_name(host, window, cx));
        return;
    }
    if let Some(host) = name.strip_prefix("connect-dup:") {
        workspace.update(cx, |workspace, cx| workspace.connect_by_name(host, window, cx));
        let workspace = workspace.clone();
        window
            .spawn(cx, async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
                workspace.update_in(cx, |workspace, window, cx| workspace.duplicate_active(window, cx)).ok();
            })
            .detach();
        return;
    }
    // `forward:<host>:<scenario>` connects the host and plays a scenario on the port forwarding panel:
    // `empty`, `live` (the rules of the host as they run), `form`, `form-d`, `form-r`, `form-clean` (the
    // new-rule form with problems and warnings), `cards`, `states`, `held`, `temporary` (cards made up for
    // the picture), `close` (the question before the last tab closes), `close-now`, `toggle=<rule name>`,
    // `toggle-off=<rule name>`, `heldport[=<rule name>]` (the server holds a remote port), `handover` (a
    // second connection, then the first tab closes), `retry=<rule name>`, `delete=<rule name>`, `reconnect`
    // (logs how the rules stand around a loss the script causes).
    // Everything is done by calling the panel and the workspace.
    if let Some(args) = name.strip_prefix("forward:") {
        let (host, scenario) = args.split_once(':').unwrap_or((args, ""));
        let (scenario, arg) = scenario.split_once('=').unwrap_or((scenario, ""));
        let (host, arg) = (host.to_string(), arg.to_string());
        workspace.update(cx, |workspace, cx| workspace.connect_by_name(&host, window, cx));
        let form = |values: &'static str| -> Vec<(u64, &'static str, String)> {
            vec![(3500, "show", String::new()), (4200, "form", values.into())]
        };
        let steps: Vec<(u64, &'static str, String)> = match scenario {
            "form" => {
                let mut steps = form("L|MySQL 主库|0.0.0.0:3306|db.internal");
                steps.push((5200, "submit-form", String::new()));
                steps
            }
            "form-d" => form("D||0.0.0.0:1080|"),
            "form-r" => form("R||0.0.0.0:9000|localhost:3000"),
            "form-clean" => form("L|MySQL 主库|5433|db.internal:5432"),
            "cards" | "states" | "held" | "temporary" => {
                vec![(3500, "show", String::new()), (4200, "cards", scenario.into())]
            }
            "close" => vec![(3500, "show", String::new()), (7000, "close", String::new())],
            "close-now" => {
                vec![(3500, "show", String::new()), (8000, "log", "before".into()), (8500, "close-now", String::new())]
            }
            "toggle" => vec![
                (3500, "show", String::new()),
                (7000, "log", "before".into()),
                (8000, "toggle-off", arg.clone()),
                (9500, "log", "off".into()),
                (11000, "toggle-on", arg.clone()),
                (13000, "log", "on".into()),
            ],
            "toggle-off" => {
                vec![(3500, "show", String::new()), (7000, "toggle-off", arg.clone()), (8500, "log", "off".into())]
            }
            // A second connection to the host; the first tab closes later and the rules move over.
            "handover" => vec![
                (3500, "show", String::new()),
                (7000, "log", "owner".into()),
                (7500, "second", String::new()),
                (10500, "show", String::new()),
                (11200, "log", "two connections".into()),
                (13500, "close-first", String::new()),
                (15500, "log", "handed over".into()),
                (16000, "log", "later".into()),
            ],
            // A remote port the server keeps holding (the script makes the server hold it and lets go again):
            // logs the states, and presses the rule's retry button (a name after `=`) at 8 s.
            "heldport" => vec![
                (2000, "log", "granted".into()),
                (3500, "show", String::new()),
                (6500, "log", "held".into()),
                (8000, "retry", arg.clone()),
                (9500, "log", "after the button".into()),
                (30000, "log", "later".into()),
            ],
            "retry" => vec![
                (3500, "show", String::new()),
                (7000, "log", "before".into()),
                (8000, "retry", arg.clone()),
                (9500, "log", "after".into()),
            ],
            "delete" => vec![(3500, "show", String::new()), (7000, "delete", arg.clone())],
            "reconnect" => vec![
                (3500, "show", String::new()),
                (7000, "log", "up".into()),
                (13000, "log", "lost".into()),
                (22000, "log", "back".into()),
                (26000, "log", "later".into()),
            ],
            _ => vec![(3500, "show", String::new()), (7000, "log", "live".into())],
        };
        let workspace = workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let mut last = 0;
                for (at_ms, step, arg) in steps {
                    cx.background_executor().timer(std::time::Duration::from_millis(at_ms - last)).await;
                    last = at_ms;
                    workspace
                        .update_in(cx, |workspace, window, cx| workspace.demo_forward_step(step, &arg, window, cx))
                        .ok();
                }
            })
            .detach();
        return;
    }
    // `monitor:<host>:<scenario>` connects the host and plays a scenario on the monitor page: `live`, `live-proc`,
    // `live-svc`, `live-dk` (the server's own numbers, on a sub-page), `fake-<page>[-<variant>]` (made-up numbers,
    // `<page>` is `ov`, `proc`, `svc` or `dk`; the variants are in `monitor::demo` and `proc-tree`, `proc-sel`,
    // `proc-term`, `proc-filter`), `gating`, `act=<action>|<command>` (a button of the process page pressed on the process
    // with that command line, `act-now=` without the question), `analyze=<mount>`, `reconnect`, `close-now`.
    if let Some(args) = name.strip_prefix("monitor:") {
        let (host, scenario) = args.split_once(':').unwrap_or((args, ""));
        let (scenario, arg) = scenario.split_once('=').unwrap_or((scenario, ""));
        let (host, arg) = (host.to_string(), arg.to_string());
        workspace.update(cx, |workspace, cx| workspace.connect_by_name(&host, window, cx));
        let show = (3500u64, "show", String::new());
        let steps: Vec<(u64, &'static str, String)> = if let Some(rest) = scenario.strip_prefix("fake-") {
            let (page, variant) = rest.split_once('-').unwrap_or((rest, ""));
            let data = match variant {
                "waiting" | "failed" | "first" | "starting" | "quiet" | "cores" | "notlinux" | "nosudo" | "alpine"
                | "forwarded" => variant,
                "coresend" => "cores",
                "term" => "nosudo",
                _ => "overview",
            };
            let data = if data == "cores" { "many-cores" } else { data };
            let mut steps = vec![show, (4000, "fake", data.into()), (4300, "page", page.into())];
            match (page, variant) {
                ("svc", _) => steps.push((4600, "svc-log", String::new())),
                ("dk", _) => steps.push((4600, "dk-log", "worker".into())),
                _ => {}
            }
            match variant {
                "coresend" => steps.push((5000, "scroll-end", String::new())),
                "tree" => steps.push((4600, "table", "|cpu|tree".into())),
                "filter" => steps.push((4600, "table", "nginx|cpu|list".into())),
                "sel" => steps.push((4600, "select", "2207".into())),
                "term" => steps.push((4600, "select", "1".into())),
                _ => {}
            }
            steps
        } else {
            match scenario {
                "live" => vec![show, (7000, "log", "overview".into()), (12000, "log", "later".into())],
                "live-proc" => vec![show, (3800, "page", "proc".into()), (9000, "log", "processes".into())],
                "live-svc" | "live-dk" => vec![show, (3800, "page", scenario[5..].into()), (6000, "log", "page".into())],
                "act" | "act-now" => vec![
                    show,
                    (3800, "page", "proc".into()),
                    (8000, "log", "before".into()),
                    (9000, scenario_step(scenario), arg.clone()),
                    (12000, "log", "after".into()),
                ],
                "analyze" => vec![show, (8000, "analyze", arg.clone())],
                // A button of the 服务 page (`<action>|<unit>`: start, stop, restart, reload, enable, disable) or of a
                // container (`<action>|<name>`: start, stop, restart, rm, exec, prune, port|<name>:<n>); `-now` skips the
                // question. The monitor logs how things stand before and after.
                "svc-act" | "svc-act-now" | "ctr-act" | "ctr-act-now" => vec![
                    show,
                    (3800, "page", if scenario.starts_with("svc") { "svc" } else { "dk" }.into()),
                    (8000, "log", "before".into()),
                    (9000, if scenario.starts_with("svc") { "svc-act" } else { "ctr-act" }, format!("{}{}", if scenario.ends_with("-now") { "!" } else { "" }, arg)),
                    (12000, "log", "after".into()),
                ],
                // The chips of the ports: the first click makes the forwards, the second says they exist.
                "ctr-fwd" => vec![
                    show,
                    (3800, "page", "dk".into()),
                    (8000, "log", "before".into()),
                    (9000, "ctr-act", "port|shellmate-web:0".into()),
                    (9500, "ctr-act", "port|shellmate-cache:0".into()),
                    (11500, "ctr-act", "port|shellmate-cache:0".into()),
                    (13000, "log", "after".into()),
                    (30000, "log", "later".into()),
                ],
                // The log under a container (and what the 查看容器 of 需要关注 does).
                "ctr-log" => vec![show, (3800, "page", "dk".into()), (5000, "show-container", arg.clone()), (12000, "log", "after".into())],
                "svc-sel" => vec![show, (3800, "page", "svc".into()), (5000, "services", arg.clone()), (12000, "log", "after".into())],
                // Collecting only while the monitor page is in front (and 10 s after): the page is shown, left for
                // the terminal, shown again within the 10 s (the collector stays), and left for good.
                "gating" => vec![
                    show,
                    (5000, "page", "proc".into()),
                    (7000, "page", "ov".into()),
                    (9000, "terminal", String::new()),
                    (14000, "show", String::new()),
                    (17000, "terminal", String::new()),
                ],
                "reconnect" => vec![
                    show,
                    (3800, "page", "proc".into()),
                    (6000, "log", "up".into()),
                    (14000, "log", "lost".into()),
                    (22000, "log", "back".into()),
                    (27000, "log", "later".into()),
                ],
                "close-now" => vec![show, (8000, "log", "before".into()), (8500, "close-now", String::new())],
                // The last tab closed while a log is followed (the Docker page with a container log, the service page with
                // the journal of a unit): nothing of it may stay on the server.
                "leak-dk" => vec![
                    show,
                    (3800, "page", "dk".into()),
                    (5000, "show-container", "shellmate-web".into()),
                    (9000, "log", "before".into()),
                    (9500, "close-now", String::new()),
                ],
                "leak-svc" => vec![
                    show,
                    (3800, "page", "svc".into()),
                    (5000, "services", "|run|shellmate-demo.service".into()),
                    (9000, "log", "before".into()),
                    (9500, "close-now", String::new()),
                ],
                _ => vec![show, (7000, "log", "live".into())],
            }
        };
        let workspace = workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let mut last = 0;
                for (at_ms, step, arg) in steps {
                    cx.background_executor().timer(std::time::Duration::from_millis(at_ms - last)).await;
                    last = at_ms;
                    workspace
                        .update_in(cx, |workspace, window, cx| workspace.demo_monitor_step(step, &arg, window, cx))
                        .ok();
                }
            })
            .detach();
        return;
    }
    // `files-refresh:<host>:<dir>` shows the panel there and refreshes it 9 seconds later (as a click
    // on 刷新 would), so that a script can kill the server's SFTP process in between.
    if let Some(args) = name.strip_prefix("files-refresh:") {
        let (host, dir) = args.split_once(':').map_or((args, None), |(host, dir)| (host, Some(dir)));
        workspace.update(cx, |workspace, cx| {
            workspace.connect_by_name(host, window, cx);
            workspace.demo_files(dir, None, false, window, cx);
        });
        let workspace = workspace.clone();
        window
            .spawn(cx, async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_secs(9)).await;
                workspace.update_in(cx, |workspace, _, cx| workspace.demo_refresh_files(cx)).ok();
            })
            .detach();
        return;
    }
    // `rail:<host>:<scenario>` connects the host and plays a scenario on the sidebar (`collapsed`,
    // `collapsed-local`, `search`, `reflow`; anything else leaves it as the saved setting has it). `reflow` logs the size
    // of every terminal before and after the sidebar is collapsed and opened again.
    if let Some(args) = name.strip_prefix("rail:") {
        let (host, scenario) = args.split_once(':').unwrap_or((args, ""));
        // `-` as the host: only the local tab.
        if host != "-" {
            workspace.update(cx, |workspace, cx| workspace.connect_by_name(host, window, cx));
        }
        let steps: &[(u64, &str)] = match scenario {
            "collapsed" => &[(500, "collapse")],
            "collapsed-local" => &[(500, "collapse"), (3500, "local")],
            "search" => &[(500, "collapse"), (3500, "search")],
            "reflow" => &[
                (3500, "log:sidebar open"),
                (3800, "collapse"),
                (5500, "log:rail"),
                (5800, "expand"),
                (7500, "log:sidebar open again"),
            ],
            _ => &[],
        };
        let workspace = workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let mut last = 0;
                for &(at_ms, step) in steps {
                    cx.background_executor().timer(std::time::Duration::from_millis(at_ms - last)).await;
                    last = at_ms;
                    workspace.update_in(cx, |workspace, window, cx| workspace.demo_rail_step(step, window, cx)).ok();
                }
            })
            .detach();
        return;
    }
    // `editor:<host>:<path>:<scenario>` opens the file in the built-in editor and plays the scenario
    // (`clean`, `modified`, `saved`, `conflict[-load|-overwrite|-third]`, `unsaved[-save|-discard]`, `refusal`, `perf`) by calling the editor,
    // not by typing: the review script changes the file on the server between the steps of `conflict`.
    if let Some(args) = name.strip_prefix("editor:") {
        let mut parts = args.splitn(3, ':').map(str::to_string);
        let (host, file, scenario) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        let dir = shellmate_sftp::path::parent(&file).unwrap_or_else(|| "/".into());
        workspace.update(cx, |workspace, cx| {
            workspace.connect_by_name(&host, window, cx);
            workspace.demo_files(Some(&dir), None, false, window, cx);
        });
        let steps: &[(u64, &str)] = match scenario.as_str() {
            "modified" => &[(4000, "open"), (6000, "type")],
            "saved" => &[(4000, "open"), (6000, "type"), (7000, "save")],
            "conflict" => &[(4000, "open"), (6000, "type"), (9500, "save")],
            "conflict-load" => &[(4000, "open"), (6000, "type"), (9500, "save"), (12000, "load")],
            "conflict-overwrite" => &[(4000, "open"), (6000, "type"), (9500, "save"), (12000, "overwrite")],
            // The script changes the file again after the dialog came up: overwriting is another conflict.
            "conflict-third" => &[(4000, "open"), (6000, "type"), (9500, "save"), (13500, "overwrite")],
            "unsaved" => &[(4000, "open"), (6000, "type"), (7000, "close")],
            "unsaved-save" => &[(4000, "open"), (6000, "type"), (7000, "close"), (9000, "unsaved-save")],
            "unsaved-discard" => &[(4000, "open"), (6000, "type"), (7000, "close"), (9000, "unsaved-discard")],
            "perf" => &[(4000, "open"), (10000, "timed")],
            _ => &[(4000, "open")],
        };
        let workspace = workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let mut last = 0;
                for &(at_ms, step) in steps {
                    cx.background_executor().timer(std::time::Duration::from_millis(at_ms - last)).await;
                    last = at_ms;
                    workspace.update_in(cx, |workspace, window, cx| workspace.demo_editor_step(step, &file, window, cx)).ok();
                }
            })
            .detach();
        return;
    }
    // `files-act:<host>:<dir>:<name>:<what>[:<text>]` shows the panel there and, a few seconds later,
    // does `what` to the entry `name` by calling the panel (`menu`, `rename-error`, `delete`, `permissions`, `drag`,
    // `download`, `newfolder`); `files-queue:<host>:<dir>` shows a transfer queue in all its states.
    // Uploads come from `$SHELLMATE_DEMO_UPLOADS`, downloads go to `$SHELLMATE_DOWNLOADS`.
    if let Some(args) = name.strip_prefix("files-act:") {
        let mut parts = args.splitn(5, ':').map(str::to_string);
        let (host, dir, entry, what, text) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        workspace.update(cx, |workspace, cx| {
            workspace.connect_by_name(&host, window, cx);
            workspace.demo_files(Some(&dir), Some(&entry), false, window, cx);
        });
        let workspace = workspace.clone();
        window
            .spawn(cx, async move |cx| {
                cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
                workspace.update_in(cx, |workspace, window, cx| workspace.demo_act(&what, &entry, &text, window, cx)).ok();
            })
            .detach();
        return;
    }
    if let Some(args) = name.strip_prefix("files-queue:") {
        let (host, dir) = args.split_once(':').unwrap_or((args, "/"));
        workspace.update(cx, |workspace, cx| {
            workspace.connect_by_name(host, window, cx);
            workspace.demo_files(Some(dir), None, false, window, cx);
        });
        let workspace = workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let mut last = 0;
                for (at_ms, step) in [(4000u64, 1u8), (6500, 2), (7000, 3), (12500, 4), (15200, 5), (17500, 6)] {
                    cx.background_executor().timer(std::time::Duration::from_millis(at_ms - last)).await;
                    last = at_ms;
                    workspace.update_in(cx, |workspace, window, cx| workspace.demo_queue_step(step, window, cx)).ok();
                }
            })
            .detach();
        return;
    }
    // `files:<host>[:<dir>[:<name to select>]]` connects the host and shows its files panel there;
    // `files-hint:<host>` shows it with the OSC 7 hint open.
    if let Some(args) = name.strip_prefix("files:").or_else(|| name.strip_prefix("files-hint:")) {
        let hint = name.starts_with("files-hint:");
        let mut parts = args.splitn(3, ':');
        let host = parts.next().unwrap_or_default().to_string();
        let (dir, select) = (parts.next().map(str::to_string), parts.next().map(str::to_string));
        workspace.update(cx, |workspace, cx| {
            workspace.connect_by_name(&host, window, cx);
            workspace.demo_files(dir.as_deref(), select.as_deref(), hint, window, cx);
        });
        return;
    }
    match name {
        "hostkey-new" => {
            let (reply, _) = oneshot::channel();
            let request = HostKeyRequest {
                host: "prod-web-03".into(),
                port: 22,
                state: FingerprintState::New,
                key: key("ssh-ed25519", "SHA256:7mJ2kQ9Lx0vT3bRk1nWc4ePZa8uYf6HsD5gVq0iN2oE"),
                recorded: vec![],
            };
            HostKeyDialog::open(request, reply, window, cx);
        }
        "hostkey-changed" => {
            let (reply, _) = oneshot::channel();
            let request = HostKeyRequest {
                host: "10.0.1.21".into(),
                port: 22,
                state: FingerprintState::Changed,
                key: key("ssh-ed25519", "SHA256:k9Fq1Zp0sW3yR7tBn2Mv6cXe8LjD4hUa5oGi0QwE1rT"),
                recorded: vec![key("ecdsa-sha2-nistp256", "SHA256:Xb4tN0q2mR8sVw1LcK7yHd3eJf9aPz5uQg6iO2nTr4E")],
            };
            HostKeyDialog::open(request, reply, window, cx);
        }
        "password" => {
            let (reply, _) = oneshot::channel();
            let request = PasswordRequest { host: "staging-api".into(), port: 22, user: "ubuntu".into(), attempt: 1 };
            SecretDialog::open_password(request, reply, window, cx);
        }
        "passphrase" => {
            let (reply, _) = oneshot::channel();
            let request = PassphraseRequest { key_path: "/Users/me/.ssh/id_ed25519".into(), attempt: 0 };
            SecretDialog::open_passphrase(request, reply, window, cx);
        }
        "otp" => {
            let (reply, _) = oneshot::channel();
            ChallengeDialog::open(challenge(2, 0, &[("Verification code: ", false)]), reply, window, cx);
        }
        "kbd" => {
            let (reply, _) = oneshot::channel();
            let mut request = challenge(1, 0, &[("Password: ", false), ("Account (optional): ", true)]);
            request.name = "服务器身份验证".into();
            request.instructions = "请输入密码，并按需填写账号。".into();
            ChallengeDialog::open(request, reply, window, cx);
        }
        "host-editor" => {
            let book = workspace.read(cx).book();
            HostEditor::open(workspace.downgrade(), EditorMode::New, &book, window, cx);
        }
        "host-editor-edit" => {
            let book = workspace.read(cx).book();
            if let Some(host) = book.hosts.iter().find(|h| h.name == "prod-db-01").cloned() {
                HostEditor::open(workspace.downgrade(), EditorMode::Edit(host), &book, window, cx);
            }
        }
        "import" => {
            let book = workspace.read(cx).book();
            let text = "Host *\n  User admin\nHost prod-web-01\n  HostName 10.0.1.21\n  User deploy\n  LocalForward 3306 10.0.2.8:3306\n  DynamicForward 1080\nHost build\n  HostName build.corp.io\n  IdentityFile ~/.ssh/id_work\n  RemoteForward 1080\nHost db-replica\n  HostName 10.0.2.9\n  User dba\n  ProxyJump bastion\n  LocalForward 5432 db.internal:5432\nHost bastion\n  HostName bastion.corp.io\n  Port 2222\n  User ops\nHost *.corp.io !build.corp.io\n  ForwardAgent yes\nInclude ~/.ssh/config.d/*\n";
            let parsed = shellmate_core::parse_ssh_config(text, std::env::home_dir().as_deref());
            let rows = shellmate_core::candidates(&parsed, &book.hosts);
            let importable = rows.iter().filter(|row| row.existing.is_none());
            let notes = ImportNotes {
                includes: parsed.includes,
                skipped_patterns: parsed.skipped_patterns,
                match_blocks: parsed.match_blocks,
                forwards: importable.clone().map(|row| row.forwards.len()).sum(),
                skipped_forwards: importable.map(|row| row.skipped_forwards).sum(),
            };
            let groups = book.groups.iter().map(|g| g.name.clone()).collect();
            ImportDialog::open(workspace.downgrade(), rows, notes, None, groups, window, cx);
        }
        "delete" => ConfirmDialog::open(
            "删除主机 prod-web-02？",
            "已打开的会话不受影响。保存在钥匙串里的此主机的密码会一并移除。",
            "删除主机",
            true,
            |_, _| Ok(()),
            window,
            cx,
        ),
        other => log::warn!("unknown SHELLMATE_DEMO {other:?}; available: {}", NAMES.join(", ")),
    }
}

/// The step a `monitor:act` scenario plays (`act` asks first, `act-now` does not).
fn scenario_step(scenario: &str) -> &'static str {
    if scenario == "act-now" { "act-now" } else { "act" }
}
