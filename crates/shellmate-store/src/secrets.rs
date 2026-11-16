//! Credential storage adapters. Tests never open the production keychain service.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use shellmate_core::{HostId, Secret};

/// Keychain service name of every Shellmate entry.
pub const KEYCHAIN_SERVICE: &str = "Shellmate";

/// What a stored secret is for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SecretKey {
    /// Stable account of a future credential type (also preserves orphaned app entries).
    Account(String),
    /// The login password of a saved host.
    HostPassword(HostId),
    /// The passphrase of a private key file.
    KeyPassphrase(PathBuf),
}

impl SecretKey {
    pub fn from_account(account: &str) -> Self {
        if let Some(id) = account
            .strip_prefix("host:")
            .and_then(|s| s.strip_suffix(":password"))
        {
            Self::HostPassword(HostId::from_raw(id))
        } else if let Some(path) = account
            .strip_prefix("key:")
            .and_then(|s| s.strip_suffix(":passphrase"))
        {
            Self::KeyPassphrase(path.into())
        } else {
            Self::Account(account.into())
        }
    }
    /// Keychain account name: `host:<id>:password` or `key:<path>:passphrase`.
    pub fn account(&self) -> String {
        match self {
            SecretKey::Account(id) => id.clone(),
            SecretKey::HostPassword(id) => format!("host:{id}:password"),
            SecretKey::KeyPassphrase(path) => format!("key:{}:passphrase", path.display()),
        }
    }
}

/// A secret store operation failed.
#[derive(Debug, thiserror::Error)]
#[error("凭据访问失败：{0}")]
pub struct SecretError(pub String);

/// Reads and writes secrets by [`SecretKey`].
pub trait SecretStore: Send + Sync {
    /// Enumerates every owned entry, including no-longer-referenced key files.
    fn keys(&self) -> Result<Vec<SecretKey>, SecretError> {
        Err(SecretError("凭据后端不支持完整枚举".into()))
    }
    /// A locked store must be explicitly unlocked; callers must not treat this as a missing secret.
    fn is_locked(&self) -> bool {
        false
    }

    /// The stored secret, or `None` if there is none.
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError>;
    /// Stores `secret`, replacing any previous one.
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError>;
    /// Removes the secret; succeeds if there is none.
    fn delete(&self, key: &SecretKey) -> Result<(), SecretError>;
}

/// The macOS keychain (service `Shellmate`).
#[derive(Clone, Copy, Debug)]
pub struct KeychainStore {
    service: &'static str,
}

impl Default for KeychainStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeychainStore {
    /// A handle to the user's keychain. Nothing is touched until a method is called.
    pub fn new() -> Self {
        Self {
            service: KEYCHAIN_SERVICE,
        }
    }

    fn entry(&self, key: &SecretKey) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(self.service, &key.account()).map_err(describe)
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
    fn keys(&self) -> Result<Vec<SecretKey>, SecretError> {
        crate::keychain_inventory::accounts(self.service).map(|accounts| {
            accounts
                .into_iter()
                .map(|a| SecretKey::from_account(&a))
                .collect()
        })
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        match self.entry(key)?.get_password() {
            Ok(password) => Ok(Some(Secret::new(password))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(describe(err)),
        }
    }

    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        self.entry(key)?
            .set_password(secret.expose())
            .map_err(describe)
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        match self.entry(key)?.delete_credential() {
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
    fn keys(&self) -> Result<Vec<SecretKey>, SecretError> {
        Ok(self.lock().keys().cloned().collect())
    }
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
        assert_eq!(
            store.get(&key).unwrap().unwrap().expose(),
            "two",
            "set replaces"
        );

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

    /// Only accesses the dedicated test service; never run automatically.
    #[test]
    #[ignore = "touches only the Shellmate Tests Only macOS keychain service"]
    fn keychain_enumeration_round_trip() {
        let store = KeychainStore {
            service: "Shellmate Tests Only",
        };
        assert!(
            store.keys().unwrap().is_empty(),
            "test service must start empty"
        );
        let keys = [
            SecretKey::Account(format!("enumeration:{}", HostId::generate())),
            SecretKey::Account(format!("枚举测试：私钥口令:{}", HostId::generate())),
        ];
        struct Cleanup<'a> {
            store: &'a KeychainStore,
            keys: &'a [SecretKey],
        }
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                for key in self.keys {
                    let _ = self.store.delete(key);
                }
            }
        }
        let cleanup = Cleanup {
            store: &store,
            keys: &keys,
        };
        for key in &keys {
            store.set(key, &Secret::new("test-only secret")).unwrap();
            assert_eq!(
                store.get(key).unwrap().unwrap().expose(),
                "test-only secret"
            );
        }
        let found = store.keys().unwrap();
        assert_eq!(found.len(), keys.len(), "no unrelated accounts may appear");
        for key in &keys {
            assert!(found.contains(key));
        }
        for entry in
            keyring_core::Entry::search(&HashMap::from([("service", store.service)])).unwrap()
        {
            let (service, account) = entry.get_specifiers().unwrap();
            assert_eq!(service, "Shellmate Tests Only");
            assert!(keys.iter().any(|key| key.account() == account));
        }
        for key in &keys {
            store.delete(key).unwrap();
        }
        assert!(store.keys().unwrap().is_empty());
        drop(cleanup);
    }
}
