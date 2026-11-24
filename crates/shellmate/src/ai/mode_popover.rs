//! Pure drawer popover coordination and permission selection policy.
use super::{commands::Popup, context_view::Popover};
use shellmate_ai::PermissionMode;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Mode,
    Context,
    Slash,
}
/// Opening any footer popup excludes the other two without changing composer text.
pub fn show(kind: Kind, mode: &mut bool, context: &mut Popover, slash: &mut Popup) {
    *mode = kind == Kind::Mode;
    if kind == Kind::Context {
        context.show();
    } else {
        context.close();
    }
    if kind != Kind::Slash {
        slash.escape();
    }
}
pub fn close(mode: &mut bool) {
    *mode = false;
}
pub fn toggle(mode: &mut bool, context: &mut Popover, slash: &mut Popup) {
    if *mode {
        close(mode);
    } else {
        show(Kind::Mode, mode, context, slash);
    }
}
/// The caller already checked that focus belongs to the drawer's composer/popover.
pub fn escape(mode: &mut bool, composing: bool) -> bool {
    if !*mode || composing {
        return false;
    }
    close(mode);
    true
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    Apply(PermissionMode),
    ConfirmFull,
}
pub fn select(open: &mut bool, current: PermissionMode, selected: PermissionMode) -> Selection {
    close(open);
    if selected == PermissionMode::Full && current != PermissionMode::Full {
        Selection::ConfirmFull
    } else {
        Selection::Apply(selected)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn open_close_toggle_escape_and_every_exclusive_transition() {
        for first in [Kind::Mode, Kind::Context, Kind::Slash] {
            for next in [Kind::Mode, Kind::Context, Kind::Slash] {
                let (mut mode, mut context, mut slash) =
                    (false, Popover::default(), Popup::default());
                if first == Kind::Slash {
                    slash.changed("/");
                }
                show(first, &mut mode, &mut context, &mut slash);
                if next == Kind::Slash {
                    slash.changed("/co");
                }
                show(next, &mut mode, &mut context, &mut slash);
                assert_eq!(
                    (mode, context.open, slash.open),
                    (
                        next == Kind::Mode,
                        next == Kind::Context,
                        next == Kind::Slash
                    )
                );
            }
        }
        let (mut mode, mut context, mut slash) = (false, Popover::default(), Popup::default());
        toggle(&mut mode, &mut context, &mut slash);
        assert!(mode);
        assert!(!escape(&mut mode, true));
        assert!(mode);
        assert!(escape(&mut mode, false));
        assert!(!mode);
        assert!(!escape(&mut mode, false));
        toggle(&mut mode, &mut context, &mut slash);
        toggle(&mut mode, &mut context, &mut slash);
        assert!(!mode);
        toggle(&mut mode, &mut context, &mut slash);
        close(&mut mode);
        assert!(!mode);
    }
    #[test]
    fn selections_close_and_persist_only_when_no_full_confirmation_is_needed() {
        use std::sync::Mutex;
        for current in [
            PermissionMode::Ask,
            PermissionMode::Auto,
            PermissionMode::Full,
        ] {
            for selected in [
                PermissionMode::Ask,
                PermissionMode::Auto,
                PermissionMode::Full,
            ] {
                let mut db = shellmate_store::HostStore::open_in_memory().unwrap();
                db.set_ai_permission("host", current).unwrap();
                let published = Mutex::new(current);
                let mut open = true;
                match select(&mut open, current, selected) {
                    Selection::ConfirmFull => {
                        assert_eq!(selected, PermissionMode::Full);
                        assert_ne!(current, PermissionMode::Full);
                        assert_eq!(db.ai_permission("host").unwrap(), current);
                        assert_eq!(*published.lock().unwrap(), current);
                    }
                    Selection::Apply(mode) => {
                        super::super::model::save_permission(&mut db, "host", &published, mode)
                            .unwrap();
                        assert_eq!(db.ai_permission("host").unwrap(), selected);
                        assert_eq!(*published.lock().unwrap(), selected);
                    }
                }
                assert!(!open);
                assert_eq!(db.ai_permission("other-host").unwrap(), PermissionMode::Ask);
            }
        }
    }
}
