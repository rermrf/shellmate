mod dialogs;
pub mod model;
pub mod password;
pub use dialogs::{PasswordDialog, Purpose, ensure_unlocked, open_import, run_operation};
#[cfg(debug_assertions)]
pub use dialogs::PreviewDialog;
use shellmate_store::{CredentialStore, SharedStore};
use std::sync::Arc;
pub fn coordinator(db: SharedStore) -> Arc<CredentialStore> {
    if crate::prototype_fixture() || (cfg!(debug_assertions) && std::env::var("SHELLMATE_DEMO").is_ok_and(|s| s.starts_with("p6:"))) {
        return fixture_coordinator(db);
    }
    Arc::new(CredentialStore::new(
        db,
        Arc::new(shellmate_store::KeychainStore::new()),
        Arc::new(shellmate_vault::biometric::NativeAuthentication),
        Arc::new(shellmate_store::LoginDeviceKeyStore),
    ))
}
// Demo and unit-test adapters never access LA or the user's keychain.
struct DemoAuth;
impl shellmate_vault::biometric::LocalAuthentication for DemoAuth {
    fn available(&self) -> Result<(), String> {
        Ok(())
    }
    fn authenticate(&self) -> Result<(), String> {
        Ok(())
    }
}
struct DemoKeys(std::sync::Mutex<std::collections::HashMap<String, zeroize::Zeroizing<Vec<u8>>>>);
impl shellmate_vault::biometric::DeviceKeyStore for DemoKeys {
    fn read(&self, id: &str) -> Result<shellmate_vault::DataKey, String> {
        let keys = self.0.lock().unwrap();
        shellmate_vault::DataKey::from_bytes(keys.get(id).ok_or("没有演示密钥")?)
            .map_err(|e| e.to_string())
    }
    fn write(&self, id: &str, key: &shellmate_vault::DataKey) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .insert(id.into(), zeroize::Zeroizing::new(key.expose().to_vec()));
        Ok(())
    }
    fn delete(&self, id: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(id);
        Ok(())
    }
}
pub fn fixture_coordinator(db: SharedStore) -> Arc<CredentialStore> {
    let manager = Arc::new(CredentialStore::new(
        db,
        Arc::new(shellmate_store::MemoryStore::new()),
        Arc::new(DemoAuth),
        Arc::new(DemoKeys(Default::default())),
    ));
    manager.refresh_touch_availability();
    manager
}
