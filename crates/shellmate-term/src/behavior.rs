//! Small terminal interaction decisions shared by the view and preference tests.
pub fn selection_copy(enabled: bool, selection: Option<String>) -> Option<String> {
    selection.filter(|_| enabled)
}
pub fn right_paste(enabled: bool) -> bool {
    enabled
}
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;
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
    fn ctrl_r_always_goes_to_the_shell() {
        let key = Keystroke::parse("ctrl-r").unwrap();
        assert_eq!(
            crate::keys::to_esc_str(&key, &Default::default()).as_deref(),
            Some("\x12")
        );
    }
}
