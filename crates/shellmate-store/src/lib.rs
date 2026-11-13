//! Persistence for Shellmate: the SQLite host library and secret storage.
//!
//! Hosts and groups live in a SQLite database (never any secret); passwords and
//! key passphrases go to the macOS keychain through [`SecretStore`].

mod error;
mod hosts;
mod secrets;

pub use error::{Result, StoreError};
pub use hosts::HostStore;
pub use secrets::{KEYCHAIN_SERVICE, KeychainStore, MemoryStore, SecretError, SecretKey, SecretStore};
