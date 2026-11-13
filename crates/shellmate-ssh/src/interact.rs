//! Callbacks through which the connection layer asks the user for decisions.
//!
//! Both traits are async and object safe: the app implements them with
//! dialogs and bridges the answer back through a oneshot channel.

use async_trait::async_trait;
use shellmate_core::FingerprintState;

use shellmate_core::Secret;

/// Algorithm and SHA-256 fingerprint of a host key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyInfo {
    /// OpenSSH algorithm name, e.g. `ssh-ed25519`.
    pub algorithm: String,
    /// OpenSSH style fingerprint, e.g. `SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s`.
    pub fingerprint: String,
}

impl HostKeyInfo {
    /// Short label for UI: `ED25519`, `RSA`, `ECDSA`, ...
    pub fn short_algorithm(&self) -> String {
        let name = self.algorithm.as_str();
        let label = match name.strip_prefix("ecdsa-sha2-") {
            Some(_) => "ECDSA",
            None => name.strip_prefix("ssh-").unwrap_or(name),
        };
        label.to_uppercase()
    }
}

/// A host key `known_hosts` cannot vouch for; the user has to decide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyRequest {
    /// Host name as configured (the `known_hosts` key).
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// [`FingerprintState::New`] (never seen) or [`FingerprintState::Changed`]
    /// (different from the recorded key of the same type: possible attack).
    pub state: FingerprintState,
    /// The key the server presented now.
    pub key: HostKeyInfo,
    /// Keys already recorded for this host: for `Changed` the ones being
    /// replaced, for `New` keys of other algorithms (if any).
    pub recorded: Vec<HostKeyInfo>,
}

/// Decides whether to trust a host key. Accepting a `New` key appends it to
/// `known_hosts`; accepting a `Changed` key replaces the recorded one.
#[async_trait]
pub trait HostKeyVerifier: Send + Sync {
    /// Returns `true` to trust the key and continue connecting.
    async fn verify(&self, request: HostKeyRequest) -> bool;
}

/// Ask for the password of `user@host`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordRequest {
    /// Host being logged in to.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Login name.
    pub user: String,
    /// 0 for the first prompt; greater than 0 when the previous answer was rejected.
    pub attempt: u32,
}

/// Ask for the passphrase of an encrypted private key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassphraseRequest {
    /// The private key file being unlocked.
    pub key_path: std::path::PathBuf,
    /// 0 for the first prompt; greater than 0 when the previous passphrase was wrong.
    pub attempt: u32,
}

/// One question of a keyboard-interactive round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChallengePrompt {
    /// The question, e.g. `Verification code: `.
    pub text: String,
    /// Whether the answer may be shown while typing (false for passwords / OTP).
    pub echo: bool,
}

/// One round of keyboard-interactive authentication. Servers may send several
/// rounds (for example password, then a verification code).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChallengeRequest {
    /// Host being logged in to.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Login name.
    pub user: String,
    /// Challenge title sent by the server (may be empty).
    pub name: String,
    /// Instructions sent by the server (may be empty).
    pub instructions: String,
    /// The questions of this round, in the order answers are expected.
    pub prompts: Vec<ChallengePrompt>,
    /// 1-based round counter within the current attempt.
    pub round: u32,
    /// 0 for the first attempt; greater than 0 when the previous attempt was rejected
    /// and the server starts over.
    pub attempt: u32,
}

/// Source of credentials the user has to type.
#[async_trait]
pub trait Prompter: Send + Sync {
    /// Returns the password, or `None` if the user cancelled.
    async fn password(&self, request: PasswordRequest) -> Option<Secret>;

    /// Returns the key passphrase, or `None` if the user cancelled.
    async fn passphrase(&self, request: PassphraseRequest) -> Option<Secret>;

    /// Returns one answer per prompt, or `None` if the user cancelled.
    async fn challenge(&self, request: ChallengeRequest) -> Option<Vec<Secret>>;
}

/// Never trusts unknown hosts and never answers prompts: for non-interactive use.
pub struct NoInteraction;

#[async_trait]
impl HostKeyVerifier for NoInteraction {
    async fn verify(&self, _: HostKeyRequest) -> bool {
        false
    }
}

#[async_trait]
impl Prompter for NoInteraction {
    async fn password(&self, _: PasswordRequest) -> Option<Secret> {
        None
    }

    async fn passphrase(&self, _: PassphraseRequest) -> Option<Secret> {
        None
    }

    async fn challenge(&self, _: ChallengeRequest) -> Option<Vec<Secret>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(algorithm: &str) -> HostKeyInfo {
        HostKeyInfo { algorithm: algorithm.into(), fingerprint: "SHA256:x".into() }
    }

    #[test]
    fn short_algorithm_labels() {
        assert_eq!(info("ssh-ed25519").short_algorithm(), "ED25519");
        assert_eq!(info("ssh-rsa").short_algorithm(), "RSA");
        assert_eq!(info("ecdsa-sha2-nistp256").short_algorithm(), "ECDSA");
    }
}
