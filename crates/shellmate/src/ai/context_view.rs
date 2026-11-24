//! Pure presentation policies for context usage; never part of the durable log.
#[derive(Default)]
pub struct Popover {
    pub open: bool,
}
impl Popover {
    pub fn show(&mut self) {
        self.open = true;
    }
    pub fn close(&mut self) {
        self.open = false;
    }
    pub fn toggle(&mut self) {
        self.open = !self.open;
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum CompactAction {
    Disabled,
    Compact,
    Queue,
}
pub fn compact_action(empty: bool, compacting: bool, busy: bool) -> CompactAction {
    if empty || compacting {
        CompactAction::Disabled
    } else if busy {
        CompactAction::Queue
    } else {
        CompactAction::Compact
    }
}
pub fn visible(bound: bool, configured: bool) -> bool {
    bound && configured
}
pub fn percent(tokens: usize, window: u32) -> String {
    let denominator = u128::from(window.max(1));
    let numerator = tokens as u128 * 100;
    if tokens > 0 && numerator < denominator {
        return "<1%".into();
    }
    format!(
        "{}%",
        ((numerator + denominator / 2) / denominator).min(100)
    )
}
pub fn tokens(n: usize) -> String {
    if n >= 1_000_000 {
        compact_number(n as f64 / 1_000_000., "M")
    } else if n >= 1000 {
        compact_number(n as f64 / 1000., "K")
    } else {
        n.to_string()
    }
}
fn compact_number(value: f64, suffix: &str) -> String {
    let number = format!("{value:.1}");
    format!("{}{suffix}", number.strip_suffix(".0").unwrap_or(&number))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percentages_tokens_and_visibility() {
        assert_eq!(tokens(73600), "73.6K");
        assert_eq!(tokens(200000), "200K");
        for (n, expected) in [
            (0, "0%"),
            (1, "<1%"),
            (1999, "<1%"),
            (2000, "1%"),
            (73600, "37%"),
            (200000, "100%"),
            (300000, "100%"),
        ] {
            assert_eq!(percent(n, 200000), expected);
        }
        assert_eq!(percent(usize::MAX, 1), "100%");
        assert!(visible(true, true));
        assert!(!visible(false, true));
        assert!(!visible(true, false));
    }
    #[test]
    fn popover_open_outside_escape_and_compact_decisions() {
        let mut p = Popover::default();
        assert!(!p.open);
        p.show();
        assert!(p.open);
        p.close();
        assert!(!p.open);
        p.toggle();
        assert!(p.open);
        p.toggle();
        assert!(!p.open);
        assert_eq!(compact_action(false, false, false), CompactAction::Compact);
        assert_eq!(compact_action(false, false, true), CompactAction::Queue);
        for busy in [false, true] {
            assert_eq!(compact_action(true, false, busy), CompactAction::Disabled);
            assert_eq!(compact_action(false, true, busy), CompactAction::Disabled);
        }
    }
}
