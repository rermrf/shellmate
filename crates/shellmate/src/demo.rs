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
use crate::dialogs::hostkey::HostKeyDialog;
use crate::workspace::Workspace;

/// Names accepted by `SHELLMATE_DEMO` (`connect:<host name>` also connects a saved host on startup).
pub const NAMES: &[&str] = &["hostkey-new", "hostkey-changed", "password", "passphrase", "otp", "kbd"];

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
        other => log::warn!("unknown SHELLMATE_DEMO {other:?}; available: {}", NAMES.join(", ")),
    }
}
