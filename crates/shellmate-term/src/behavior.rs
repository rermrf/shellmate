//! Small terminal interaction decisions shared by the view and preference tests.
use gpui::Keystroke;
pub fn selection_copy(enabled: bool, selection: Option<String>) -> Option<String> {
    selection.filter(|_| enabled)
}
pub fn right_paste(enabled: bool) -> bool {
    enabled
}
pub fn history_shortcut(enabled: bool, key: &Keystroke) -> bool {
    enabled
        && key.key == "r"
        && key.modifiers.control
        && !key.modifiers.platform
        && !key.modifiers.alt
        && !key.modifiers.shift
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_copy_toggle_controls_clipboard_payload() {
        assert_eq!(
            selection_copy(true, Some("selected".into())),
            Some("selected".into())
        );
        assert_eq!(selection_copy(false, Some("selected".into())), None);
        assert_eq!(selection_copy(true, None), None);
    }
    #[test]
    fn right_paste_toggle_controls_action() {
        assert!(right_paste(true));
        assert!(!right_paste(false));
    }
    #[test]
    fn shortcut_off_passes_ctrl_r_to_shell() {
        let key = Keystroke::parse("ctrl-r").unwrap();
        assert!(history_shortcut(true, &key));
        assert!(!history_shortcut(false, &key));
        assert_eq!(
            crate::keys::to_esc_str(&key, &Default::default()).as_deref(),
            Some("\x12")
        );
        for key in ["cmd-r", "ctrl-shift-r", "r"] {
            assert!(!history_shortcut(true, &Keystroke::parse(key).unwrap()));
        }
    }
}
