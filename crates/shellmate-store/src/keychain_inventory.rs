//! Enumerate through the same default store that keyring uses for writes.
use crate::SecretError;
use std::collections::HashMap;

pub fn accounts(service: &str) -> Result<Vec<String>, SecretError> {
    let fail = || SecretError("无法枚举 Shellmate 钥匙串条目；未迁移任何凭据".into());
    // keyring-core has no default store until keyring's adapter initializes it.
    keyring::Entry::store_status()
        .as_ref()
        .map_err(|_| fail())?;
    keyring_core::Entry::search(&HashMap::from([("service", service)]))
        .map_err(|_| fail())?
        .into_iter()
        .map(|entry| {
            let (found_service, account) = entry.get_specifiers().ok_or_else(fail)?;
            if found_service != service {
                return Err(fail());
            }
            Ok(account)
        })
        .collect()
}
