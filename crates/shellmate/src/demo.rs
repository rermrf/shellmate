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
            let text = "Host *\n  User admin\nHost prod-web-01\n  HostName 10.0.1.21\n  User deploy\nHost build\n  HostName build.corp.io\n  IdentityFile ~/.ssh/id_work\nHost db-replica\n  HostName 10.0.2.9\n  User dba\n  ProxyJump bastion\nHost bastion\n  HostName bastion.corp.io\n  Port 2222\n  User ops\nHost *.corp.io !build.corp.io\n  ForwardAgent yes\nInclude ~/.ssh/config.d/*\n";
            let parsed = shellmate_core::parse_ssh_config(text, std::env::home_dir().as_deref());
            let rows = shellmate_core::candidates(&parsed, &book.hosts);
            let notes = ImportNotes {
                includes: parsed.includes,
                skipped_patterns: parsed.skipped_patterns,
                match_blocks: parsed.match_blocks,
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
