//! The LA gate is separate from the device-key store: the unsigned macOS app
//! uses the login keychain, not a biometry ACL. Replace the adapter once signed.
use crate::DataKey;
pub trait LocalAuthentication: Send + Sync {
    fn available(&self) -> Result<(), String>;
    /// Called on a worker. The native completion arrives through a channel.
    fn authenticate(&self) -> Result<(), String>;
}
pub trait DeviceKeyStore: Send + Sync {
    fn read(&self, account: &str) -> Result<DataKey, String>;
    fn write(&self, account: &str, key: &DataKey) -> Result<(), String>;
    fn delete(&self, account: &str) -> Result<(), String>;
}
pub fn unlock(
    auth: &dyn LocalAuthentication,
    store: &dyn DeviceKeyStore,
    account: &str,
) -> Result<DataKey, String> {
    auth.available()?;
    auth.authenticate()?;
    store.read(account)
}
pub struct NativeAuthentication;
#[cfg(target_os = "macos")]
impl LocalAuthentication for NativeAuthentication {
    fn available(&self) -> Result<(), String> {
        use objc2_local_authentication::{LAContext, LAPolicy};
        let ctx = unsafe { LAContext::new() };
        unsafe { ctx.canEvaluatePolicy_error(LAPolicy::DeviceOwnerAuthenticationWithBiometrics) }
            .map_err(|_| "此设备未设置 Touch ID，或当前不可用".into())
    }
    fn authenticate(&self) -> Result<(), String> {
        use objc2_foundation::{NSError, NSString};
        use objc2_local_authentication::{LAContext, LAPolicy};
        let ctx = unsafe { LAContext::new() };
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let reply =
            block2::RcBlock::new(move |success: objc2::runtime::Bool, _error: *mut NSError| {
                let _ = tx.send(success.as_bool());
            });
        unsafe {
            ctx.evaluatePolicy_localizedReason_reply(
                LAPolicy::DeviceOwnerAuthenticationWithBiometrics,
                &NSString::from_str("解锁 Shellmate 凭据库"),
                &reply,
            )
        };
        match rx.recv() {
            Ok(true) => Ok(()),
            _ => Err("Touch ID 验证已取消或未通过".into()),
        }
    }
}
#[cfg(not(target_os = "macos"))]
impl LocalAuthentication for NativeAuthentication {
    fn available(&self) -> Result<(), String> {
        Err("此平台不支持 Touch ID".into())
    }
    fn authenticate(&self) -> Result<(), String> {
        self.available()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Auth(bool, bool);
    impl LocalAuthentication for Auth {
        fn available(&self) -> Result<(), String> {
            if self.0 {
                Ok(())
            } else {
                Err("unavailable".into())
            }
        }
        fn authenticate(&self) -> Result<(), String> {
            if self.1 { Ok(()) } else { Err("cancel".into()) }
        }
    }
    struct Keys(AtomicUsize);
    impl DeviceKeyStore for Keys {
        fn read(&self, _: &str) -> Result<DataKey, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(DataKey::generate().unwrap())
        }
        fn write(&self, _: &str, _: &DataKey) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    #[test]
    fn read_only_after_success() {
        let k = Keys(AtomicUsize::new(0));
        assert!(unlock(&Auth(false, true), &k, "id").is_err());
        assert!(unlock(&Auth(true, false), &k, "id").is_err());
        assert_eq!(k.0.load(Ordering::SeqCst), 0);
        assert!(unlock(&Auth(true, true), &k, "id").is_ok());
        assert_eq!(k.0.load(Ordering::SeqCst), 1);
    }
}
