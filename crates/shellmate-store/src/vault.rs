//! One shared SQLite connection; a serialized credential coordinator prevents
//! backend switches racing with a connection's credential reads/writes.
use crate::{HostStore, SecretError, SecretKey, SecretStore};
use rusqlite::{OptionalExtension, params};
use shellmate_core::Secret;
use shellmate_vault::{
    Clock, DataKey, EncryptedEntry, IdleLock, KdfParams, MonotonicClock,
    biometric::{DeviceKeyStore, LocalAuthentication},
};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn error(e: impl std::fmt::Display) -> SecretError {
    SecretError(e.to_string())
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
#[derive(Clone)]
pub struct SharedStore(Arc<Mutex<HostStore>>);
impl SharedStore {
    pub fn new(store: HostStore) -> Self {
        Self(Arc::new(Mutex::new(store)))
    }
    pub fn borrow(&self) -> MutexGuard<'_, HostStore> {
        lock(&self.0)
    }
    pub fn borrow_mut(&self) -> MutexGuard<'_, HostStore> {
        lock(&self.0)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Keychain,
    Master,
}
#[derive(Clone, Debug)]
pub struct SecurityStatus {
    pub backend: Backend,
    pub locked: bool,
    pub idle_lock: bool,
    pub touch_id: bool,
    pub touch_available: Result<(), String>,
}
/// Standalone vault-backed SecretStore; key ownership is shared with the coordinator.
pub struct VaultStore {
    pub(crate) db: SharedStore,
    pub(crate) key: Mutex<Option<DataKey>>,
    unlocked: AtomicBool,
}
impl VaultStore {
    pub fn new(db: SharedStore) -> Self {
        Self {
            db,
            key: Mutex::new(None),
            unlocked: AtomicBool::new(false),
        }
    }
    pub fn lock(&self) {
        *lock(&self.key) = None;
        self.unlocked.store(false, Ordering::SeqCst);
    }
    fn wrapped_key(&self) -> Result<Vec<u8>, SecretError> {
        let (version,salt,memory,iterations,parallelism,blob):(u32,Vec<u8>,u32,u32,u32,Vec<u8>)=self.db.borrow().conn.query_row("SELECT format_version,salt,memory_kib,iterations,parallelism,wrapped_key FROM vault_meta WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).map_err(error)?;
        let (params, wrapped_salt) = shellmate_vault::wrapped_parameters(&blob).map_err(error)?;
        if version != shellmate_vault::FORMAT_VERSION
            || params
                != (KdfParams {
                    memory_kib: memory,
                    iterations,
                    parallelism,
                })
            || salt != wrapped_salt
        {
            return Err(error("凭据库元数据损坏或版本不支持"));
        }
        Ok(blob)
    }
    pub fn unlock(&self, password: &str) -> Result<(), SecretError> {
        let key = DataKey::unwrap(password, &self.wrapped_key()?).map_err(error)?;
        self.install_key(key)
    }
    fn install_key(&self, key: DataKey) -> Result<(), SecretError> {
        let (nonce, ciphertext): (Vec<u8>, Vec<u8>) = self
            .db
            .borrow()
            .conn
            .query_row(
                "SELECT check_nonce,check_ciphertext FROM vault_meta WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(error)?;
        let entry = EncryptedEntry {
            nonce: nonce.try_into().map_err(|_| error("凭据库已损坏"))?,
            ciphertext,
        };
        if key
            .decrypt("Shellmate vault verifier", &entry)
            .map_err(error)?
            .as_slice()
            != b"Shellmate vault v1"
        {
            return Err(error("凭据库密钥不匹配"));
        }
        *lock(&self.key) = Some(key);
        self.unlocked.store(true, Ordering::SeqCst);
        Ok(())
    }
    pub(crate) fn encrypt(
        &self,
        key: &SecretKey,
        secret: &Secret,
    ) -> Result<EncryptedEntry, SecretError> {
        lock(&self.key)
            .as_ref()
            .ok_or_else(|| error("凭据库已锁定，请先解锁"))?
            .encrypt(&key.account(), secret.expose().as_bytes())
            .map_err(error)
    }
}
impl SecretStore for VaultStore {
    fn keys(&self) -> Result<Vec<SecretKey>, SecretError> {
        let db = self.db.borrow();
        let mut q = db
            .conn
            .prepare("SELECT credential_id FROM vault_secrets ORDER BY credential_id")
            .map_err(error)?;
        q.query_map([], |r| r.get::<_, String>(0))
            .map_err(error)?
            .map(|r| r.map(|s| SecretKey::from_account(&s)).map_err(error))
            .collect()
    }
    fn is_locked(&self) -> bool {
        !self.unlocked.load(Ordering::SeqCst)
    }
    fn get(&self, id: &SecretKey) -> Result<Option<Secret>, SecretError> {
        let key = lock(&self.key);
        let key = key
            .as_ref()
            .ok_or_else(|| error("凭据库已锁定，请先解锁"))?;
        let raw: Option<(Vec<u8>, Vec<u8>)> = self
            .db
            .borrow()
            .conn
            .query_row(
                "SELECT nonce,ciphertext FROM vault_secrets WHERE credential_id=?1",
                [id.account()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(error)?;
        raw.map(|(n, c)| {
            let entry = EncryptedEntry {
                nonce: n.try_into().map_err(|_| error("凭据格式损坏"))?,
                ciphertext: c,
            };
            let bytes = key.decrypt(&id.account(), &entry).map_err(error)?;
            let s = std::str::from_utf8(&bytes).map_err(|_| error("凭据编码损坏"))?;
            Ok(Secret::new(s))
        })
        .transpose()
    }
    fn set(&self, id: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        let entry = self.encrypt(id, secret)?;
        write_secret(&self.db.borrow().conn, id, &entry)
    }
    fn delete(&self, id: &SecretKey) -> Result<(), SecretError> {
        if self.is_locked() {
            return Err(error("凭据库已锁定，请先解锁"));
        }
        self.db
            .borrow()
            .conn
            .execute(
                "DELETE FROM vault_secrets WHERE credential_id=?1",
                [id.account()],
            )
            .map_err(error)?;
        Ok(())
    }
}
pub(crate) fn write_secret(
    conn: &rusqlite::Connection,
    id: &SecretKey,
    e: &EncryptedEntry,
) -> Result<(), SecretError> {
    conn.execute("INSERT INTO vault_secrets VALUES(?1,?2,?3) ON CONFLICT(credential_id) DO UPDATE SET nonce=excluded.nonce,ciphertext=excluded.ciphertext",params![id.account(),e.nonce.as_slice(),e.ciphertext]).map_err(error)?;
    Ok(())
}

pub struct CredentialStore {
    pub(crate) db: SharedStore,
    pub(crate) vault: VaultStore,
    pub(crate) external: Arc<dyn SecretStore>,
    pub(crate) operation: Mutex<()>,
    master: AtomicBool,
    touch: AtomicBool,
    idle: Mutex<IdleLock>,
    auth: Arc<dyn LocalAuthentication>,
    availability: Mutex<Option<std::result::Result<(), String>>>,
    device: Arc<dyn DeviceKeyStore>,
}
impl CredentialStore {
    pub fn new(
        db: SharedStore,
        external: Arc<dyn SecretStore>,
        auth: Arc<dyn LocalAuthentication>,
        device: Arc<dyn DeviceKeyStore>,
    ) -> Self {
        Self::with_clock(db, external, auth, device, Box::<MonotonicClock>::default())
    }
    pub fn with_clock(
        db: SharedStore,
        external: Arc<dyn SecretStore>,
        auth: Arc<dyn LocalAuthentication>,
        device: Arc<dyn DeviceKeyStore>,
        clock: Box<dyn Clock>,
    ) -> Self {
        let mut idle = IdleLock::new(clock);
        idle.enabled = db
            .borrow()
            .setting("vault_idle_lock")
            .ok()
            .flatten()
            .as_deref()
            != Some("0");
        let master = {
            let stored = db.borrow();
            stored
                .setting("credential_backend")
                .map_or(true, |v| v.as_deref() == Some("master"))
                || stored
                    .conn
                    .query_row("SELECT EXISTS(SELECT 1 FROM vault_meta)", [], |r| {
                        r.get::<_, bool>(0)
                    })
                    .unwrap_or(true)
        };
        let touch = db
            .borrow()
            .setting("vault_touch_id")
            .ok()
            .flatten()
            .as_deref()
            == Some("1");
        Self {
            vault: VaultStore::new(db.clone()),
            db,
            external,
            operation: Mutex::new(()),
            master: AtomicBool::new(master),
            touch: AtomicBool::new(touch),
            idle: Mutex::new(idle),
            auth,
            availability: Mutex::new(None),
            device,
        }
    }
    pub fn backend(&self) -> Backend {
        if self.master.load(Ordering::SeqCst) {
            Backend::Master
        } else {
            Backend::Keychain
        }
    }
    pub fn status(&self) -> SecurityStatus {
        SecurityStatus {
            backend: self.backend(),
            locked: self.is_locked(),
            idle_lock: lock(&self.idle).enabled,
            touch_id: self.touch_enabled(),
            touch_available: lock(&self.availability)
                .clone()
                .unwrap_or_else(|| Err("正在检查 Touch ID…".into())),
        }
    }
    /// Call on a worker; settings/rendering only read the cached result.
    pub fn refresh_touch_availability(&self) {
        *lock(&self.availability) = Some(self.auth.available());
    }
    pub fn activity(&self) {
        lock(&self.idle).activity();
    }
    pub fn tick(&self) -> bool {
        let Ok(_op) = self.operation.try_lock() else {
            return false;
        };
        if lock(&self.idle).expired() && !self.vault.is_locked() {
            self.vault.lock();
            true
        } else {
            false
        }
    }
    pub fn lock(&self) {
        let _op = lock(&self.operation);
        self.vault.lock();
    }
    pub fn unlock(&self, password: &str) -> Result<(), SecretError> {
        let _op = lock(&self.operation);
        self.vault.unlock(password)?;
        self.activity();
        Ok(())
    }
    pub fn set_idle_lock(&self, enabled: bool) -> Result<(), SecretError> {
        self.db
            .borrow_mut()
            .set_setting("vault_idle_lock", if enabled { "1" } else { "0" })
            .map_err(error)?;
        let mut idle = lock(&self.idle);
        idle.enabled = enabled;
        idle.activity();
        Ok(())
    }
    fn touch_enabled(&self) -> bool {
        self.touch.load(Ordering::SeqCst)
    }
    fn device_id(&self) -> Result<String, SecretError> {
        self.db
            .borrow()
            .conn
            .query_row("SELECT device_id FROM vault_meta WHERE id=1", [], |r| {
                r.get(0)
            })
            .map_err(error)
    }
    pub fn set_touch_id(&self, enabled: bool) -> Result<(), SecretError> {
        let _op = lock(&self.operation);
        let id = self.device_id()?;
        if enabled {
            self.auth.available().map_err(error)?;
            let key = lock(&self.vault.key);
            let key = key.as_ref().ok_or_else(|| error("请先解锁凭据库"))?;
            self.device.write(&id, key).map_err(error)?;
        } else {
            self.device.delete(&id).map_err(error)?;
        }
        if let Err(e) = self
            .db
            .borrow_mut()
            .set_setting("vault_touch_id", if enabled { "1" } else { "0" })
        {
            if enabled {
                let _ = self.device.delete(&id);
            }
            return Err(error(e));
        }
        self.touch.store(enabled, Ordering::SeqCst);
        Ok(())
    }
    pub fn unlock_touch_id(&self) -> Result<(), SecretError> {
        let _op = lock(&self.operation);
        if !self.touch_enabled() {
            return Err(error("尚未启用 Touch ID"));
        }
        let key = shellmate_vault::biometric::unlock(
            self.auth.as_ref(),
            self.device.as_ref(),
            &self.device_id()?,
        )
        .map_err(error)?;
        self.vault.install_key(key)?;
        self.activity();
        Ok(())
    }
    pub fn change_password(
        &self,
        current_password: &str,
        password: &str,
        params: KdfParams,
    ) -> Result<(), SecretError> {
        let _op = lock(&self.operation);
        if self.vault.is_locked() {
            return Err(error("请先解锁凭据库"));
        }
        let key = DataKey::unwrap(current_password, &self.vault.wrapped_key()?).map_err(|e| {
            if e == shellmate_vault::Error::Authentication {
                error("当前主密码不正确")
            } else {
                error(e)
            }
        })?;
        let wrapped = key.wrap(password, params).map_err(error)?;
        let (_, salt) = shellmate_vault::wrapped_parameters(&wrapped).map_err(error)?;
        self.db.borrow().conn.execute("UPDATE vault_meta SET salt=?1,memory_kib=?2,iterations=?3,parallelism=?4,wrapped_key=?5,changed_at=?6 WHERE id=1",params![salt,params.memory_kib,params.iterations,params.parallelism,wrapped,now()]).map_err(error)?;
        Ok(())
    }
    /// Copies + verifies every owned key before deleting any source. A SQLite
    /// transaction keeps partial vaults invisible; external mutations are compensated.
    pub fn switch_to_master(&self, password: &str, params: KdfParams) -> Result<(), SecretError> {
        let _op = lock(&self.operation);
        if self.backend() == Backend::Master {
            return Err(error("已使用主密码凭据库"));
        }
        let source = read_all(self.external.as_ref())?;
        let key = DataKey::generate().map_err(error)?;
        let wrapped = key.wrap(password, params).map_err(error)?;
        let (_, salt) = shellmate_vault::wrapped_parameters(&wrapped).map_err(error)?;
        let check = key
            .encrypt("Shellmate vault verifier", b"Shellmate vault v1")
            .map_err(error)?;
        let mut db = self.db.borrow_mut();
        let tx = db.conn.transaction().map_err(error)?;
        tx.execute(
            "INSERT INTO vault_meta VALUES(1,1,?1,?2,?3,?4,?5,?6,?6,?7,?8,?9)",
            params![
                salt,
                params.memory_kib,
                params.iterations,
                params.parallelism,
                wrapped,
                now(),
                uuid::Uuid::new_v4().to_string(),
                check.nonce.as_slice(),
                check.ciphertext
            ],
        )
        .map_err(error)?;
        for (id, secret) in &source {
            let e = key
                .encrypt(&id.account(), secret.expose().as_bytes())
                .map_err(error)?;
            write_secret(&tx, id, &e)?;
            let (n, c): (Vec<u8>, Vec<u8>) = tx
                .query_row(
                    "SELECT nonce,ciphertext FROM vault_secrets WHERE credential_id=?1",
                    [id.account()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(error)?;
            let e = EncryptedEntry {
                nonce: n.try_into().map_err(|_| error("迁移验证失败"))?,
                ciphertext: c,
            };
            if key.decrypt(&id.account(), &e).map_err(error)?.as_slice()
                != secret.expose().as_bytes()
            {
                return Err(error("迁移验证失败"));
            }
        }
        tx.execute("INSERT INTO settings VALUES('credential_backend','master') ON CONFLICT(key) DO UPDATE SET value='master'",[]).map_err(error)?;
        // Commit the complete, verified encrypted copy before deleting source
        // items. A process crash during deletion must never lose both copies.
        tx.commit().map_err(error)?;
        drop(db);
        let deletion = source
            .iter()
            .try_for_each(|(id, _)| self.external.delete(id));
        if let Err(original) = deletion {
            let restored = write_verified(self.external.as_ref(), &source).is_ok();
            if restored {
                let rollback = (|| {
                    let mut db = self.db.borrow_mut();
                    let tx = db.conn.transaction().map_err(error)?;
                    tx.execute_batch("DELETE FROM vault_secrets; DELETE FROM vault_meta; UPDATE settings SET value='keychain' WHERE key='credential_backend';").map_err(error)?;
                    tx.commit().map_err(error)
                })();
                if rollback.is_ok() {
                    return Err(original);
                }
            }
            // Preserve the complete destination when source recovery is itself
            // refused. Removing it here would destroy the only remaining copy.
            *lock(&self.vault.key) = Some(key);
            self.vault.unlocked.store(true, Ordering::SeqCst);
            self.master.store(true, Ordering::SeqCst);
            self.activity();
            return Err(error(
                "迁移清理失败，钥匙串恢复未完成；完整凭据已保留在主密码库，请勿重置凭据库",
            ));
        }
        *lock(&self.vault.key) = Some(key);
        self.vault.unlocked.store(true, Ordering::SeqCst);
        self.master.store(true, Ordering::SeqCst);
        self.activity();
        Ok(())
    }
    pub fn switch_to_keychain(&self) -> Result<(), SecretError> {
        let _op = lock(&self.operation);
        if self.backend() != Backend::Master {
            return Ok(());
        }
        if self.vault.is_locked() {
            return Err(error("请先解锁凭据库"));
        }
        let source = read_all(&self.vault)?;
        let before = snapshot(self.external.as_ref(), source.iter().map(|(k, _)| k))?;
        if let Err(e) = write_verified(self.external.as_ref(), &source) {
            return Err(restore_options(self.external.as_ref(), &before, e));
        }
        // Delete the convenience key before removing metadata; failure leaves the vault intact.
        if let Err(e) = self.device.delete(&self.device_id()?) {
            return Err(restore_options(self.external.as_ref(), &before, error(e)));
        }
        let result = (|| {
            let mut db = self.db.borrow_mut();
            let tx = db.conn.transaction().map_err(error)?;
            tx.execute_batch("DELETE FROM vault_secrets; DELETE FROM vault_meta; INSERT INTO settings VALUES('credential_backend','keychain') ON CONFLICT(key) DO UPDATE SET value='keychain'; INSERT INTO settings VALUES('vault_touch_id','0') ON CONFLICT(key) DO UPDATE SET value='0';").map_err(error)?;
            tx.commit().map_err(error)
        })();
        if let Err(e) = result {
            let original = restore_options(self.external.as_ref(), &before, e);
            if self.touch_enabled() {
                let restored = self.device_id().and_then(|id| {
                    let key = lock(&self.vault.key);
                    self.device
                        .write(&id, key.as_ref().ok_or_else(|| error("凭据库已锁定"))?)
                        .map_err(error)
                });
                if restored.is_err() {
                    return Err(error(
                        "切换失败，凭据仍在主密码库；Touch ID 设备密钥未能恢复，请使用主密码解锁并重新启用 Touch ID",
                    ));
                }
            }
            return Err(original);
        }
        self.vault.lock();
        self.master.store(false, Ordering::SeqCst);
        self.touch.store(false, Ordering::SeqCst);
        Ok(())
    }
    /// Hosts and configuration are retained. Call only after explicit destructive confirmation.
    pub fn reset(&self) -> Result<(), SecretError> {
        let _op = lock(&self.operation);
        if let Ok(id) = self.device_id() {
            self.device.delete(&id).map_err(error)?;
        }
        let mut db = self.db.borrow_mut();
        let tx = db.conn.transaction().map_err(error)?;
        tx.execute_batch("DELETE FROM vault_secrets; DELETE FROM vault_meta; INSERT INTO settings VALUES('credential_backend','keychain') ON CONFLICT(key) DO UPDATE SET value='keychain'; INSERT INTO settings VALUES('vault_touch_id','0') ON CONFLICT(key) DO UPDATE SET value='0';").map_err(error)?;
        tx.commit().map_err(error)?;
        drop(db);
        self.vault.lock();
        self.master.store(false, Ordering::SeqCst);
        self.touch.store(false, Ordering::SeqCst);
        Ok(())
    }
    pub(crate) fn active(&self) -> &dyn SecretStore {
        if self.backend() == Backend::Master {
            &self.vault
        } else {
            self.external.as_ref()
        }
    }
}
impl SecretStore for CredentialStore {
    fn keys(&self) -> Result<Vec<SecretKey>, SecretError> {
        let _op = lock(&self.operation);
        self.active().keys()
    }
    fn is_locked(&self) -> bool {
        self.backend() == Backend::Master && self.vault.is_locked()
    }
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        let _op = lock(&self.operation);
        self.active().get(key)
    }
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        let _op = lock(&self.operation);
        self.active().set(key, secret)
    }
    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        let _op = lock(&self.operation);
        self.active().delete(key)
    }
}
pub(crate) fn read_all(store: &dyn SecretStore) -> Result<Vec<(SecretKey, Secret)>, SecretError> {
    if store.is_locked() {
        return Err(error("凭据库已锁定，请先解锁"));
    }
    store
        .keys()?
        .into_iter()
        .filter_map(|k| match store.get(&k) {
            Ok(Some(s)) => Some(Ok((k, s))),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        })
        .collect()
}
pub(crate) fn snapshot<'a>(
    store: &dyn SecretStore,
    keys: impl Iterator<Item = &'a SecretKey>,
) -> Result<Vec<(SecretKey, Option<Secret>)>, SecretError> {
    keys.map(|k| Ok((k.clone(), store.get(k)?))).collect()
}
pub(crate) fn write_verified(
    store: &dyn SecretStore,
    items: &[(SecretKey, Secret)],
) -> Result<(), SecretError> {
    for (k, s) in items {
        store.set(k, s)?;
        if store.get(k)?.as_ref() != Some(s) {
            return Err(error("凭据回读验证失败"));
        }
    }
    Ok(())
}
pub(crate) fn restore_options(
    store: &dyn SecretStore,
    items: &[(SecretKey, Option<Secret>)],
    original: SecretError,
) -> SecretError {
    let mut failed = false;
    for (k, s) in items {
        let r = match s {
            Some(s) => store.set(k, s),
            None => store.delete(k),
        };
        failed |= r.is_err();
    }
    if failed {
        error("操作失败，且源凭据恢复不完整；请勿继续迁移，请检查钥匙串访问权限")
    } else {
        original
    }
}
/// Login-keychain item, local to this device. No iCloud/data-protection store or biometry ACL.
/// LA is an application gate; other processes with keychain access may read the key.
pub struct LoginDeviceKeyStore;
impl DeviceKeyStore for LoginDeviceKeyStore {
    fn read(&self, id: &str) -> Result<DataKey, String> {
        let bytes = zeroize::Zeroizing::new(
            keyring::Entry::new("Shellmate Vault Key", id)
                .map_err(|_| "无法访问设备密钥")?
                .get_secret()
                .map_err(|_| "无法读取设备密钥，请用主密码解锁")?,
        );
        DataKey::from_bytes(&bytes).map_err(|e| e.to_string())
    }
    fn write(&self, id: &str, key: &DataKey) -> Result<(), String> {
        keyring::Entry::new("Shellmate Vault Key", id)
            .map_err(|_| "无法访问设备密钥")?
            .set_secret(key.expose())
            .map_err(|_| "无法保存设备密钥".into())
    }
    fn delete(&self, id: &str) -> Result<(), String> {
        match keyring::Entry::new("Shellmate Vault Key", id)
            .map_err(|_| "无法访问设备密钥")?
            .delete_credential()
        {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err("无法删除设备密钥".into()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::MemoryStore;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    pub struct Auth;
    impl LocalAuthentication for Auth {
        fn available(&self) -> Result<(), String> {
            Ok(())
        }
        fn authenticate(&self) -> Result<(), String> {
            Ok(())
        }
    }
    #[derive(Default)]
    pub struct Keys(Mutex<HashMap<String, zeroize::Zeroizing<Vec<u8>>>>);
    use std::collections::HashMap;
    impl DeviceKeyStore for Keys {
        fn read(&self, id: &str) -> Result<DataKey, String> {
            let values = lock(&self.0);
            DataKey::from_bytes(values.get(id).ok_or("missing")?).map_err(|e| e.to_string())
        }
        fn write(&self, id: &str, key: &DataKey) -> Result<(), String> {
            lock(&self.0).insert(id.into(), zeroize::Zeroizing::new(key.expose().to_vec()));
            Ok(())
        }
        fn delete(&self, id: &str) -> Result<(), String> {
            lock(&self.0).remove(id);
            Ok(())
        }
    }
    pub fn setup() -> (SharedStore, Arc<MemoryStore>, CredentialStore) {
        let db = SharedStore::new(HostStore::open_in_memory().unwrap());
        let external = Arc::new(MemoryStore::new());
        let manager = CredentialStore::new(
            db.clone(),
            external.clone(),
            Arc::new(Auth),
            Arc::new(Keys::default()),
        );
        (db, external, manager)
    }
    pub fn fast() -> KdfParams {
        KdfParams {
            memory_kib: 8192,
            iterations: 1,
            parallelism: 1,
        }
    }
    fn key(n: usize) -> SecretKey {
        SecretKey::Account(format!("owned:{n}"))
    }
    #[test]
    fn switch_roundtrip_lock_rewrap_touch_reset() {
        let (db, external, m) = setup();
        let host = shellmate_core::Host::new("kept", "example.test");
        db.borrow_mut().save_host(&host).unwrap();
        for n in 0..3 {
            external
                .set(&key(n), &Secret::new(format!("secret-{n}")))
                .unwrap();
        }
        m.switch_to_master("Initial9!Meadow", fast()).unwrap();
        assert!(external.keys().unwrap().is_empty());
        assert_eq!(m.keys().unwrap().len(), 3);
        m.set_touch_id(true).unwrap();
        m.change_password("Initial9!Meadow", "Replacement9!River", fast())
            .unwrap();
        m.lock();
        assert!(m.get(&key(0)).is_err());
        assert!(m.unlock("Initial9!Meadow").is_err());
        m.unlock_touch_id().unwrap();
        assert_eq!(m.get(&key(0)).unwrap().unwrap().expose(), "secret-0");
        m.switch_to_keychain().unwrap();
        assert_eq!(external.keys().unwrap().len(), 3);
        assert_eq!(
            db.borrow()
                .conn
                .query_row::<i64, _, _>("SELECT count(*) FROM vault_secrets", [], |r| r.get(0))
                .unwrap(),
            0
        );
        m.switch_to_master("NewMaster9!Stone", fast()).unwrap();
        m.lock();
        m.reset().unwrap();
        assert_eq!(m.backend(), Backend::Keychain);
        assert!(m.keys().unwrap().is_empty());
        assert!(db.borrow().host(&host.id).unwrap().is_some());
    }
    #[test]
    fn changing_password_authenticates_current_metadata_and_keeps_secrets() {
        let (db, external, m) = setup();
        external
            .set(&key(0), &Secret::new("retained credential"))
            .unwrap();
        m.switch_to_master("Initial9!Meadow", fast()).unwrap();
        let metadata = || {
            let db = db.borrow();
            db.conn
                .query_row(
                    "SELECT wrapped_key,changed_at,salt FROM vault_meta WHERE id=1",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, Vec<u8>>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, Vec<u8>>(2)?,
                        ))
                    },
                )
                .unwrap()
        };
        let encrypted = || {
            let db = db.borrow();
            db.conn
                .query_row("SELECT nonce,ciphertext FROM vault_secrets", [], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .unwrap()
        };
        let before = metadata();
        let secret_before = encrypted();
        assert_eq!(
            m.change_password("wrong current", "Replacement9!River", fast())
                .unwrap_err()
                .0,
            "当前主密码不正确"
        );
        assert_eq!(metadata(), before);
        assert_eq!(encrypted(), secret_before);
        assert_eq!(
            m.get(&key(0)).unwrap().unwrap().expose(),
            "retained credential"
        );
        m.change_password("Initial9!Meadow", "Replacement9!River", fast())
            .unwrap();
        let changed = metadata();
        assert_ne!(changed.0, before.0);
        assert_eq!(encrypted(), secret_before);
        // The unlocked key is unchanged. Only the new password authenticates the persisted wrapper.
        assert_eq!(
            m.change_password("Initial9!Meadow", "Another9!Meadow", fast())
                .unwrap_err()
                .0,
            "当前主密码不正确"
        );
        assert_eq!(metadata(), changed);
        m.lock();
        m.unlock("Replacement9!River").unwrap();
        assert_eq!(
            m.get(&key(0)).unwrap().unwrap().expose(),
            "retained credential"
        );
    }
    struct FailStore {
        inner: MemoryStore,
        writes: AtomicUsize,
        fail_at: AtomicUsize,
        deletes: AtomicUsize,
        fail_delete: AtomicUsize,
    }
    impl Default for FailStore {
        fn default() -> Self {
            Self {
                inner: MemoryStore::new(),
                writes: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(usize::MAX),
                deletes: AtomicUsize::new(0),
                fail_delete: AtomicUsize::new(usize::MAX),
            }
        }
    }
    impl SecretStore for FailStore {
        fn keys(&self) -> Result<Vec<SecretKey>, SecretError> {
            self.inner.keys()
        }
        fn get(&self, k: &SecretKey) -> Result<Option<Secret>, SecretError> {
            self.inner.get(k)
        }
        fn set(&self, k: &SecretKey, s: &Secret) -> Result<(), SecretError> {
            if self.writes.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_at.load(Ordering::SeqCst)
            {
                return Err(error("injected write failure"));
            }
            self.inner.set(k, s)
        }
        fn delete(&self, k: &SecretKey) -> Result<(), SecretError> {
            if self.deletes.fetch_add(1, Ordering::SeqCst) + 1
                == self.fail_delete.load(Ordering::SeqCst)
            {
                return Err(error("injected delete failure"));
            }
            self.inner.delete(k)
        }
    }
    #[test]
    fn keychain_destination_nth_write_rolls_back() {
        for fail in 1..=3 {
            let db = SharedStore::new(HostStore::open_in_memory().unwrap());
            let external = Arc::new(FailStore::default());
            for n in 0..3 {
                external.inner.set(&key(n), &Secret::new("source")).unwrap();
            }
            let m = CredentialStore::new(
                db,
                external.clone(),
                Arc::new(Auth),
                Arc::new(Keys::default()),
            );
            m.switch_to_master("Master9!Meadow", fast()).unwrap();
            external.fail_at.store(fail, Ordering::SeqCst);
            assert!(m.switch_to_keychain().is_err());
            assert_eq!(m.backend(), Backend::Master);
            assert!(external.keys().unwrap().is_empty());
            assert_eq!(m.keys().unwrap().len(), 3);
            for n in 0..3 {
                assert_eq!(m.get(&key(n)).unwrap().unwrap().expose(), "source");
            }
        }
    }
    #[test]
    fn source_delete_failure_restores_keychain_and_removes_partial_vault() {
        let db = SharedStore::new(HostStore::open_in_memory().unwrap());
        let external = Arc::new(FailStore::default());
        for n in 0..3 {
            external.inner.set(&key(n), &Secret::new("source")).unwrap();
        }
        external.fail_delete.store(2, Ordering::SeqCst);
        let m = CredentialStore::new(
            db.clone(),
            external.clone(),
            Arc::new(Auth),
            Arc::new(Keys::default()),
        );
        assert!(m.switch_to_master("Master9!Meadow", fast()).is_err());
        assert_eq!(m.backend(), Backend::Keychain);
        assert_eq!(external.keys().unwrap().len(), 3);
        assert_eq!(
            db.borrow()
                .conn
                .query_row::<i64, _, _>("SELECT count(*) FROM vault_meta", [], |r| r.get(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn vault_destination_write_failure_keeps_keychain() {
        let (db, external, m) = setup();
        for n in 0..3 {
            external.set(&key(n), &Secret::new("source")).unwrap();
        }
        db.borrow().conn.execute_batch("CREATE TRIGGER fail_vault BEFORE INSERT ON vault_secrets WHEN (SELECT count(*) FROM vault_secrets)=1 BEGIN SELECT RAISE(ABORT,'injected second write'); END;").unwrap();
        assert!(m.switch_to_master("Master9!Meadow", fast()).is_err());
        assert_eq!(m.backend(), Backend::Keychain);
        assert_eq!(external.keys().unwrap().len(), 3);
        assert_eq!(
            db.borrow()
                .conn
                .query_row::<i64, _, _>("SELECT count(*) FROM vault_meta", [], |r| r.get(0))
                .unwrap(),
            0
        );
    }
    struct FakeClock(Arc<AtomicU64>);
    impl Clock for FakeClock {
        fn now(&self) -> std::time::Duration {
            std::time::Duration::from_secs(self.0.load(Ordering::SeqCst))
        }
    }
    #[test]
    fn idle_lock_erases_key_activity_extends_timer() {
        let (db, external, _) = setup();
        let time = Arc::new(AtomicU64::new(0));
        let m = CredentialStore::with_clock(
            db,
            external,
            Arc::new(Auth),
            Arc::new(Keys::default()),
            Box::new(FakeClock(time.clone())),
        );
        m.switch_to_master("Master9!Meadow", fast()).unwrap();
        time.store(899, Ordering::SeqCst);
        assert!(!m.tick());
        m.activity();
        time.store(900, Ordering::SeqCst);
        assert!(!m.tick());
        time.store(1799, Ordering::SeqCst);
        assert!(m.tick());
        assert!(lock(&m.vault.key).is_none());
        m.unlock("Master9!Meadow").unwrap();
        assert!(!m.tick());
    }
    #[test]
    fn migrates_v6_keeps_everything_and_adds_stable_ids() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::hosts::migrate_for_tests(&mut conn, 6);
        conn.execute_batch("INSERT INTO hosts(id,name,addr,port,protocol,auth) VALUES('old','old','old.test',22,'ssh','agent'); INSERT INTO snippet_groups(id,name,sort) VALUES(1,'ops',0); INSERT INTO snippets(group_id,name,command,sort) VALUES(1,'list','ls',0); INSERT INTO settings VALUES('custom','retained');").unwrap();
        crate::hosts::migrate_for_tests(&mut conn, 7);
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            7
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM hosts", [], |r| r.get(0))
                .unwrap(),
            1
        );
        assert!(
            !conn
                .query_row::<String, _, _>("SELECT stable_id FROM snippets", [], |r| r.get(0))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT value FROM settings WHERE key='custom'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            "retained"
        );
    }
}
