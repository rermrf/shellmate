//! Secret storage: the macOS keychain in the app, memory in tests.
//!
//! Nothing here (or anywhere in the host database) writes secrets to disk
//! outside the keychain.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use shellmate_core::{HostId, Secret};

/// Keychain service name of every Shellmate entry.
pub const KEYCHAIN_SERVICE: &str = "Shellmate";

/// What a stored secret is for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SecretKey {
    /// The login password of a saved host.
    HostPassword(HostId),
    /// The passphrase of a private key file.
    KeyPassphrase(PathBuf),
}

impl SecretKey {
    /// Keychain account name: `host:<id>:password` or `key:<path>:passphrase`.
    pub fn account(&self) -> String {
        match self {
            SecretKey::HostPassword(id) => format!("host:{id}:password"),
            SecretKey::KeyPassphrase(path) => format!("key:{}:passphrase", path.display()),
        }
    }
}

/// A secret store operation failed.
#[derive(Debug, thiserror::Error)]
#[error("钥匙串访问失败：{0}")]
pub struct SecretError(pub String);

/// Reads and writes secrets by [`SecretKey`].
pub trait SecretStore: Send + Sync {
    /// The stored secret, or `None` if there is none.
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError>;
    /// Stores `secret`, replacing any previous one.
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError>;
    /// Removes the secret; succeeds if there is none.
    fn delete(&self, key: &SecretKey) -> Result<(), SecretError>;
}

/// The macOS keychain (service `Shellmate`).
#[derive(Clone, Copy, Debug, Default)]
pub struct KeychainStore;

impl KeychainStore {
    /// A handle to the user's keychain. Nothing is touched until a method is called.
    pub fn new() -> Self {
        Self
    }

    fn entry(key: &SecretKey) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(KEYCHAIN_SERVICE, &key.account()).map_err(describe)
    }
}

/// Chinese description of a keyring error that never includes secret data.
fn describe(err: keyring::Error) -> SecretError {
    use keyring::Error as E;
    SecretError(match err {
        E::NoStorageAccess(_) => "无法访问钥匙串（可能已锁定，或访问被拒绝）".into(),
        E::PlatformFailure(err) => format!("系统钥匙串返回错误：{err}"),
        E::NoDefaultStore => "当前系统没有可用的钥匙串".into(),
        E::BadEncoding(_) | E::BadDataFormat(..) => "钥匙串中的条目格式无法识别".into(),
        E::Ambiguous(_) => "钥匙串中存在多个同名条目".into(),
        E::TooLong(..) | E::Invalid(..) => "钥匙串条目名称无效".into(),
        E::NoEntry => "钥匙串中没有这个条目".into(),
        _ => "未知错误".into(),
    })
}

impl SecretStore for KeychainStore {
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        match Self::entry(key)?.get_password() {
            Ok(password) => Ok(Some(Secret::new(password))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(describe(err)),
        }
    }

    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        Self::entry(key)?.set_password(secret.expose()).map_err(describe)
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(describe(err)),
        }
    }
}

/// Secrets held in memory only, for tests and as a fallback when the keychain is unusable.
#[derive(Debug, Default)]
pub struct MemoryStore {
    secrets: Mutex<HashMap<SecretKey, Secret>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SecretKey, Secret>> {
        self.secrets.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        Ok(self.lock().get(key).cloned())
    }

    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        self.lock().insert(key.clone(), secret.clone());
        Ok(())
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        self.lock().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn password_key() -> SecretKey {
        SecretKey::HostPassword(HostId::from_raw("0a1b2c3d"))
    }

    #[test]
    fn accounts_follow_the_documented_format() {
        assert_eq!(password_key().account(), "host:0a1b2c3d:password");
        assert_eq!(
            SecretKey::KeyPassphrase("/Users/me/.ssh/id_ed25519".into()).account(),
            "key:/Users/me/.ssh/id_ed25519:passphrase"
        );
    }

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryStore::new();
        let key = password_key();
        assert!(store.get(&key).unwrap().is_none());

        store.set(&key, &Secret::new("one")).unwrap();
        assert_eq!(store.get(&key).unwrap().unwrap().expose(), "one");
        store.set(&key, &Secret::new("two")).unwrap();
        assert_eq!(store.get(&key).unwrap().unwrap().expose(), "two", "set replaces");

        store.delete(&key).unwrap();
        assert!(store.get(&key).unwrap().is_none());
        store.delete(&key).unwrap();
    }

    #[test]
    fn memory_store_keeps_keys_apart() {
        let store = MemoryStore::new();
        let host = password_key();
        let key = SecretKey::KeyPassphrase("/k".into());
        store.set(&host, &Secret::new("host-secret")).unwrap();
        store.set(&key, &Secret::new("key-secret")).unwrap();
        assert_eq!(store.get(&host).unwrap().unwrap().expose(), "host-secret");
        assert_eq!(store.get(&key).unwrap().unwrap().expose(), "key-secret");
        store.delete(&host).unwrap();
        assert!(store.get(&key).unwrap().is_some());
    }

    #[test]
    fn usable_as_a_trait_object() {
        let store: Box<dyn SecretStore> = Box::new(MemoryStore::new());
        store.set(&password_key(), &Secret::new("x")).unwrap();
        assert!(store.get(&password_key()).unwrap().is_some());
    }

    /// Talks to the real macOS keychain (may show an access prompt): run on demand with
    /// `cargo test -p shellmate-store -- --ignored keychain`.
    #[test]
    #[ignore = "touches the real macOS keychain"]
    fn keychain_round_trip() {
        let store = KeychainStore::new();
        let key = SecretKey::HostPassword(HostId::generate());
        assert!(store.get(&key).unwrap().is_none());
        store.set(&key, &Secret::new("s3cret")).unwrap();
        assert_eq!(store.get(&key).unwrap().unwrap().expose(), "s3cret");
        store.delete(&key).unwrap();
        assert!(store.get(&key).unwrap().is_none());
        store.delete(&key).unwrap();
    }
}
