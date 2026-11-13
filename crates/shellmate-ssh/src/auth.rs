//! Authentication of one hop: ssh-agent, private key, password and
//! keyboard-interactive, in the order configured for the host.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{Algorithm, PrivateKey, PrivateKeyWithHashAlg, decode_secret_key};
use russh::{MethodKind, MethodSet};
use shellmate_core::AuthMethod;
use zeroize::Zeroizing;

use crate::config::SshHost;
use crate::error::SshError;
use crate::handler::ClientHandler;
use crate::interact::{ChallengePrompt, ChallengeRequest, PassphraseRequest, PasswordRequest, Prompter};
use shellmate_core::Secret;

/// Wrong password / passphrase / challenge answers allowed before giving up.
const MAX_ATTEMPTS: u32 = 3;

/// File names of the keys tried when no identity file is configured.
const DEFAULT_KEYS: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

pub(crate) struct AuthContext<'a> {
    pub prompter: &'a dyn Prompter,
    pub ssh_dir: &'a Path,
    pub agent_socket: Option<&'a Path>,
}

enum Attempt {
    Success,
    /// The server said no. `remaining` are the methods it still accepts;
    /// `partial` means it wants a further factor.
    Rejected {
        remaining: MethodSet,
        partial: bool,
    },
}

fn kind_of(method: AuthMethod) -> MethodKind {
    match method {
        AuthMethod::PrivateKey | AuthMethod::Agent => MethodKind::PublicKey,
        AuthMethod::Password => MethodKind::Password,
        AuthMethod::KeyboardInteractive => MethodKind::KeyboardInteractive,
    }
}

fn method_names(set: &MethodSet) -> Vec<String> {
    set.iter().map(|kind| <&str>::from(kind).to_string()).collect()
}

/// Authenticates `host.user` on `handle`. Returns the method that worked, or
/// `None` if the server did not ask for credentials at all.
pub(crate) async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    host: &SshHost,
    ctx: &AuthContext<'_>,
) -> Result<Option<AuthMethod>, SshError> {
    // Probe: learns which methods the server offers (and whether it wants any).
    let mut offered = match handle.authenticate_none(&host.user).await? {
        AuthResult::Success => return Ok(None),
        AuthResult::Failure { remaining_methods, .. } => remaining_methods,
    };

    let mut plan: VecDeque<AuthMethod> = host.auth.iter().copied().collect();
    let mut tried: Vec<AuthMethod> = Vec::new();
    let mut first_error: Option<SshError> = None;

    while let Some(method) = plan.pop_front() {
        if tried.contains(&method) {
            continue;
        }
        tried.push(method);
        // The first configured method is the primary one; the others are fallbacks.
        let primary = host.auth.first() == Some(&method);
        if !offered.is_empty() && !offered.contains(&kind_of(method)) {
            first_error.get_or_insert(SshError::AuthMethodUnavailable { method, offered: method_names(&offered) });
            continue;
        }
        let attempt = match method {
            AuthMethod::Password => try_password(handle, host, ctx).await,
            AuthMethod::PrivateKey => try_private_key(handle, host, ctx).await,
            AuthMethod::Agent => try_agent(handle, host, ctx).await,
            AuthMethod::KeyboardInteractive => try_keyboard_interactive(handle, host, ctx).await,
        };
        match attempt {
            Ok(Attempt::Success) => return Ok(Some(method)),
            Ok(Attempt::Rejected { remaining, partial }) => {
                first_error.get_or_insert(SshError::AuthRejected(method));
                if !remaining.is_empty() {
                    offered = remaining.clone();
                }
                if partial {
                    // Multi-factor: the server accepted this step and wants another one.
                    for kind in remaining.iter() {
                        match kind {
                            MethodKind::Password => plan.push_front(AuthMethod::Password),
                            MethodKind::KeyboardInteractive => plan.push_front(AuthMethod::KeyboardInteractive),
                            MethodKind::PublicKey => plan.push_front(AuthMethod::PrivateKey),
                            _ => {}
                        }
                    }
                }
            }
            // The user cancelled a prompt of the configured method: stop. Cancelling the prompt of a
            // fallback only stops trying; what is reported is how the configured method failed.
            Err(SshError::AuthCancelled) => {
                return Err(first_error.filter(|_| !primary).unwrap_or(SshError::AuthCancelled));
            }
            // A key file the user configured explicitly must be usable; a broken default key must not
            // get in the way of the fallbacks.
            Err(err @ SshError::KeyUnreadable { .. }) if primary => return Err(err),
            Err(err) => {
                first_error.get_or_insert(err);
            }
        }
    }
    Err(first_error.unwrap_or(SshError::NoAuthMethod))
}

async fn try_password(
    handle: &mut Handle<ClientHandler>,
    host: &SshHost,
    ctx: &AuthContext<'_>,
) -> Result<Attempt, SshError> {
    for attempt in 0..MAX_ATTEMPTS {
        let secret = match (&host.password, attempt) {
            (Some(known), 0) => known.clone(),
            _ => ctx
                .prompter
                .password(PasswordRequest {
                    host: host.host.clone(),
                    port: host.port,
                    user: host.user.clone(),
                    attempt,
                })
                .await
                .ok_or(SshError::AuthCancelled)?,
        };
        match handle.authenticate_password(&host.user, secret.expose().to_string()).await? {
            AuthResult::Success => return Ok(Attempt::Success),
            AuthResult::Failure { remaining_methods, partial_success } => {
                let retry = !partial_success && remaining_methods.contains(&MethodKind::Password);
                if !retry || attempt + 1 == MAX_ATTEMPTS {
                    return Ok(Attempt::Rejected { remaining: remaining_methods, partial: partial_success });
                }
            }
        }
    }
    Ok(Attempt::Rejected { remaining: MethodSet::empty(), partial: false })
}

/// Key files to try: the configured one (an error if unreadable) or the default keys that exist.
fn key_candidates(host: &SshHost, ctx: &AuthContext<'_>) -> Vec<(PathBuf, bool)> {
    match &host.identity_file {
        Some(path) => vec![(path.clone(), true)],
        None => DEFAULT_KEYS
            .iter()
            .map(|name| ctx.ssh_dir.join(name))
            .filter(|path| path.is_file())
            .map(|path| (path, false))
            .collect(),
    }
}

async fn try_private_key(
    handle: &mut Handle<ClientHandler>,
    host: &SshHost,
    ctx: &AuthContext<'_>,
) -> Result<Attempt, SshError> {
    let candidates = key_candidates(host, ctx);
    if candidates.is_empty() {
        let searched = DEFAULT_KEYS.iter().map(|name| ctx.ssh_dir.join(name).display().to_string()).collect();
        return Err(SshError::NoPrivateKey { searched });
    }
    let mut last = Attempt::Rejected { remaining: MethodSet::empty(), partial: false };
    let mut loaded_any = false;
    let mut skipped: Option<SshError> = None;
    for (path, explicit) in candidates {
        let key = match load_key(&path, host, ctx).await {
            Ok(key) => key,
            Err(err @ SshError::AuthCancelled) => return Err(err),
            Err(err) if explicit => return Err(err),
            Err(err) => {
                // A default key we cannot use must not prevent trying the others.
                log::debug!("skipping default key {}: {err}", path.display());
                skipped.get_or_insert(err);
                continue;
            }
        };
        loaded_any = true;
        let hash = if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
            handle.best_supported_rsa_hash().await?.flatten()
        } else {
            None
        };
        match handle.authenticate_publickey(&host.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await? {
            AuthResult::Success => return Ok(Attempt::Success),
            AuthResult::Failure { remaining_methods, partial_success } => {
                last = Attempt::Rejected { remaining: remaining_methods, partial: partial_success };
                if partial_success {
                    break;
                }
            }
        }
    }
    match (loaded_any, skipped) {
        (false, Some(err)) => Err(err),
        _ => Ok(last),
    }
}

/// Reads a private key, asking for the passphrase when it is encrypted.
async fn load_key(path: &Path, host: &SshHost, ctx: &AuthContext<'_>) -> Result<PrivateKey, SshError> {
    let unreadable = |reason: String| SshError::KeyUnreadable { path: path.to_path_buf(), reason };
    // The file may hold an unencrypted private key: wipe our copies when done.
    let text = Zeroizing::new(tokio::fs::read_to_string(path).await.map_err(|err| unreadable(err.to_string()))?);
    let decode = |passphrase: Option<Secret>, text: Zeroizing<String>| {
        // Key derivation (bcrypt) is CPU heavy: keep it off the async workers.
        tokio::task::spawn_blocking(move || decode_secret_key(&text, passphrase.as_ref().map(Secret::expose)))
    };

    match decode(None, text.clone()).await.map_err(|err| unreadable(err.to_string()))? {
        Ok(key) => return Ok(key),
        Err(russh::keys::Error::KeyIsEncrypted) => {}
        Err(err) => return Err(unreadable(describe_key_error(&err))),
    }

    for attempt in 0..MAX_ATTEMPTS {
        let passphrase = match (&host.passphrase, attempt) {
            (Some(known), 0) => known.clone(),
            _ => ctx
                .prompter
                .passphrase(PassphraseRequest { key_path: path.to_path_buf(), attempt })
                .await
                .ok_or(SshError::AuthCancelled)?,
        };
        if let Ok(key) = decode(Some(passphrase), text.clone()).await.map_err(|err| unreadable(err.to_string()))? {
            return Ok(key);
        }
    }
    Err(unreadable("私钥口令不正确".into()))
}

fn describe_key_error(err: &russh::keys::Error) -> String {
    match err {
        russh::keys::Error::CouldNotReadKey | russh::keys::Error::KeyIsCorrupt => "文件不是有效的私钥".into(),
        russh::keys::Error::UnsupportedKeyType { key_type_string, .. } => format!("不支持的密钥类型 {key_type_string}"),
        other => other.to_string(),
    }
}

async fn try_agent(
    handle: &mut Handle<ClientHandler>,
    host: &SshHost,
    ctx: &AuthContext<'_>,
) -> Result<Attempt, SshError> {
    let mut agent = match ctx.agent_socket {
        Some(socket) => AgentClient::connect_uds(socket).await,
        None => AgentClient::connect_env().await,
    }
    .map_err(|err| SshError::Agent(describe_agent_error(&err)))?;
    let identities = agent.request_identities().await.map_err(|err| SshError::Agent(describe_agent_error(&err)))?;
    let keys: Vec<_> = identities
        .into_iter()
        .filter_map(|identity| match identity {
            AgentIdentity::PublicKey { key, .. } => Some(key),
            AgentIdentity::Certificate { .. } => None,
        })
        .collect();
    if keys.is_empty() {
        return Err(SshError::Agent("ssh-agent 中没有可用的密钥".into()));
    }

    let mut last = Attempt::Rejected { remaining: MethodSet::empty(), partial: false };
    for key in keys {
        let hash = if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
            handle.best_supported_rsa_hash().await?.flatten()
        } else {
            None
        };
        match handle.authenticate_publickey_with(&host.user, key, hash, &mut agent).await {
            Ok(AuthResult::Success) => return Ok(Attempt::Success),
            Ok(AuthResult::Failure { remaining_methods, partial_success }) => {
                last = Attempt::Rejected { remaining: remaining_methods, partial: partial_success };
                if partial_success {
                    break;
                }
            }
            // The agent could not sign with this key (e.g. confirmation denied): try the next.
            Err(err) => log::debug!("ssh-agent could not sign: {err}"),
        }
    }
    Ok(last)
}

fn describe_agent_error(err: &russh::keys::Error) -> String {
    match err {
        russh::keys::Error::EnvVar(_) => "未检测到 ssh-agent（环境变量 SSH_AUTH_SOCK 未设置）".into(),
        russh::keys::Error::BadAuthSock => "SSH_AUTH_SOCK 指向的 ssh-agent 套接字不存在".into(),
        russh::keys::Error::IO(io) => format!("无法连接 ssh-agent：{io}"),
        other => format!("ssh-agent 出错：{other}"),
    }
}

async fn try_keyboard_interactive(
    handle: &mut Handle<ClientHandler>,
    host: &SshHost,
    ctx: &AuthContext<'_>,
) -> Result<Attempt, SshError> {
    for attempt in 0..MAX_ATTEMPTS {
        let mut round = 0;
        let mut response = handle.authenticate_keyboard_interactive_start(&host.user, None).await?;
        loop {
            match response {
                KeyboardInteractiveAuthResponse::Success => return Ok(Attempt::Success),
                KeyboardInteractiveAuthResponse::Failure { remaining_methods, partial_success } => {
                    let retry = !partial_success && remaining_methods.contains(&MethodKind::KeyboardInteractive);
                    if !retry || attempt + 1 == MAX_ATTEMPTS {
                        return Ok(Attempt::Rejected { remaining: remaining_methods, partial: partial_success });
                    }
                    break;
                }
                KeyboardInteractiveAuthResponse::InfoRequest { name, instructions, prompts } => {
                    round += 1;
                    let answers: Vec<String> = if prompts.is_empty() {
                        Vec::new()
                    } else {
                        let request = ChallengeRequest {
                            host: host.host.clone(),
                            port: host.port,
                            user: host.user.clone(),
                            name,
                            instructions,
                            prompts: prompts
                                .into_iter()
                                .map(|p| ChallengePrompt { text: p.prompt, echo: p.echo })
                                .collect(),
                            round,
                            attempt,
                        };
                        let expected = request.prompts.len();
                        let answers = ctx.prompter.challenge(request).await.ok_or(SshError::AuthCancelled)?;
                        if answers.len() != expected {
                            return Err(SshError::AuthCancelled);
                        }
                        answers.iter().map(|a| a.expose().to_string()).collect()
                    };
                    response = handle.authenticate_keyboard_interactive_respond(answers).await?;
                }
            }
        }
    }
    Ok(Attempt::Rejected { remaining: MethodSet::empty(), partial: false })
}
