//! Settings and dialog policy without gpui.
use shellmate_store::{Backend, SecurityStatus};
pub const RESET_WARNING: &str = "将永久删除凭据库中保存的全部主机密码、私钥口令和其他凭据，以及 Touch ID 设备密钥。主机、分组、快捷命令和转发规则保留。没有恢复方式，此操作无法撤销。";
pub const TOUCH_TRADEOFF: &str = "Touch ID 会在本机登录钥匙串中保存解锁密钥。读取前由 Shellmate 验证指纹；拥有钥匙串访问权限的其他程序可能读取该密钥。";
pub struct SettingsModel {
    pub master: bool,
    pub locked: bool,
    pub touch_enabled: bool,
    pub touch_reason: Option<String>,
    pub idle_enabled: bool,
}
impl SettingsModel {
    pub fn new(s: SecurityStatus) -> Self {
        let master = s.backend == Backend::Master;
        Self {
            master,
            locked: s.locked,
            touch_enabled: s.touch_id,
            touch_reason: if !master {
                Some("先设置主密码凭据库".into())
            } else {
                s.touch_available.err()
            },
            idle_enabled: s.idle_lock,
        }
    }
    pub fn can_lock(&self) -> bool {
        self.master && !self.locked
    }
    pub fn can_change_password(&self) -> bool {
        self.master
    }
}
pub fn validate_password(password: &str, confirmation: &str) -> Result<(), String> {
    if password != confirmation {
        Err("两次输入的口令不一致".into())
    } else if !shellmate_vault::acceptable(password) {
        Err(shellmate_vault::PASSWORD_MINIMUM.into())
    } else {
        Ok(())
    }
}
/// Weak legacy passwords may still unlock/import; only newly chosen passwords
/// must meet the current strength policy.
pub fn password_submit_enabled(password: &str, creates_password: bool, busy: bool) -> bool {
    !busy && (!creates_password || shellmate_vault::acceptable(password))
}
/// Keep original row indices: sorting the visible list must not change the
/// import plan's dependency order or send a conflict choice to a different item.
pub fn preview_order(
    classes: impl Iterator<Item = shellmate_store::bundle::Classification>,
) -> Vec<usize> {
    use shellmate_store::bundle::Classification;
    let mut rows = classes.enumerate().collect::<Vec<_>>();
    rows.sort_by_key(|(_, class)| match class {
        Classification::Conflict => 0,
        Classification::New => 1,
        Classification::Identical => 2,
    });
    rows.into_iter().map(|(index, _)| index).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_states() {
        let mut s = SecurityStatus {
            backend: Backend::Keychain,
            locked: false,
            idle_lock: true,
            touch_id: false,
            touch_available: Ok(()),
        };
        assert!(SettingsModel::new(s.clone()).touch_reason.is_some());
        s.backend = Backend::Master;
        let m = SettingsModel::new(s.clone());
        assert!(m.can_lock() && m.can_change_password() && m.idle_enabled);
        s.locked = true;
        s.touch_available = Err("不可用".into());
        let m = SettingsModel::new(s);
        assert!(!m.can_lock());
        assert_eq!(m.touch_reason.as_deref(), Some("不可用"));
    }
    #[test]
    fn confirmation_and_minimum() {
        assert!(validate_password("long passphrase", "different").is_err());
        assert!(validate_password("a", "a").is_err());
        for weak in ["1234567890", "aaaaaaaaaaaaaaaa", "Password123!"] {
            assert_eq!(
                validate_password(weak, weak).unwrap_err(),
                shellmate_vault::PASSWORD_MINIMUM
            );
        }
        assert!(validate_password("long passphrase", "long passphrase").is_ok());
    }
    #[test]
    fn new_password_buttons_require_medium_but_legacy_unlock_is_allowed() {
        assert!(!password_submit_enabled("1234567890", true, false));
        assert!(!password_submit_enabled("aaaaaaaaaaaaaaaa", true, false));
        assert!(password_submit_enabled("zR7!kT2?", true, false));
        assert!(password_submit_enabled("old weak password", false, false));
        assert!(!password_submit_enabled("Zebra9!Rocks?", true, true));
    }
    #[test]
    fn preview_prioritizes_conflicts_without_changing_choice_indices() {
        use shellmate_store::bundle::Classification::{Conflict, Identical, New};
        assert_eq!(
            preview_order([Identical, New, Conflict, Identical, Conflict, New].into_iter()),
            [2, 4, 1, 5, 0, 3]
        );
    }
}
