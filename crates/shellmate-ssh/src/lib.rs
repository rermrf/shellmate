//! SSH client core for Shellmate: connections (direct and ProxyJump), host key
//! verification against `known_hosts`, authentication, and shell channels that
//! plug into the terminal as a [`TerminalBackend`](shellmate_core::TerminalBackend).
//!
//! The crate does not depend on the UI. Decisions that need a human go through
//! the async [`HostKeyVerifier`] and [`Prompter`] traits; progress is reported
//! as [`ConnectEvent`]s. Everything runs on the [`SshRuntime`].

#![warn(missing_docs)]

mod auth;
mod config;
mod connection;
mod error;
mod exec;
mod forward;
mod handler;
mod interact;
mod known_hosts;
mod key_info;
mod runtime;
mod shell;
mod subsystem;
mod watchdog;

pub use config::{ConnectEvent, ConnectOptions, EventCallback, HostSecrets, Route, SshHost};
pub use connection::{Connection, HopInfo, WeakConnection};
pub use error::{SshError, TcpipFailure};
pub use exec::{DEFAULT_MAX_LINE, DEFAULT_MAX_OUTPUT, ExecOptions, ExecOutput, ExecStatus, ExecStream, StreamOptions};
pub use forward::{ForwardedStream, RemoteForward, TcpipStream};
pub use interact::{
    ChallengePrompt, ChallengeRequest, HostKeyDecision, HostKeyInfo, HostKeyRequest, HostKeyVerifier, NoInteraction, PassphraseRequest,
    PasswordRequest, Prompter,
};
pub use key_info::{KeyInfo, randomart};
pub use known_hosts::{KnownHosts, Lookup};
pub use runtime::SshRuntime;
pub use shell::{PendingShell, ShellChannel, TERM};
pub use subsystem::SubsystemStream;
pub use shellmate_core::Secret;
