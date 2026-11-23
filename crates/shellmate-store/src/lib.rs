//! Persistence for Shellmate: the SQLite host library and secret storage.
//!
//! Hosts and groups live in SQLite. Credentials use the selected [`SecretStore`]:
//! the macOS keychain or authenticated ciphertext in the same SQLite library.

mod error;
mod forwards;
mod hosts;
mod productivity;
mod secrets;

pub use error::{Result, StoreError};
pub use hosts::HostStore;
pub use secrets::{KEYCHAIN_SERVICE, KeychainStore, MemoryStore, SecretError, SecretKey, SecretStore};

pub use productivity::{Snippet, SnippetGroup, HistoryEntry, HISTORY_ENABLED, HISTORY_SHORTCUT, HISTORY_LIMIT};

mod recordings;
pub use recordings::{Recording,Preference,Preferences,CloseWindowWithSessions,TerminalFontSize};

mod vault;
pub use vault::{SharedStore, CredentialStore, VaultStore, Backend, SecurityStatus};
pub mod bundle;

mod keychain_inventory;
pub use vault::LoginDeviceKeyStore;

pub mod ai;
