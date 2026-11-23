//! Focus reporting (DECSET 1004): tells the program when its terminal gains or loses the focus.
//!
//! Whether the terminal has the focus is the owner's to say (it is a matter of windows, tabs and
//! panes); this only decides what to send. A program that turned reporting on gets `ESC [ I` when
//! the focus arrives and `ESC [ O` when it leaves, each state once. Turning reporting on while the
//! terminal has no focus reports that at once, as the program would otherwise assume it has it.

/// What the program is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusReport {
    In,
    Out,
}

impl FocusReport {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::In => b"\x1b[I",
            Self::Out => b"\x1b[O",
        }
    }

    pub fn focused(self) -> bool {
        self == Self::In
    }
}

#[derive(Debug, Default)]
pub struct FocusReporter {
    focused: bool,
    /// The state the program last knows about; `None` while reporting is off.
    reported: Option<bool>,
}

impl FocusReporter {
    /// Records whether the terminal has the focus. Nothing is sent until [`poll`](Self::poll).
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    /// The report to send now, given whether the program has reporting turned on, if its state is
    /// not what the program last heard. Call after the focus or the mode may have changed.
    pub fn poll(&mut self, reporting: bool) -> Option<FocusReport> {
        if !reporting {
            self.reported = None;
            return None;
        }
        match self.reported {
            Some(known) if known == self.focused => None,
            // Just turned on and focused: the program's own assumption is right.
            None if self.focused => {
                self.reported = Some(true);
                None
            }
            _ => {
                self.reported = Some(self.focused);
                Some(if self.focused { FocusReport::In } else { FocusReport::Out })
            }
        }
    }

    /// A new backend knows nothing yet.
    pub fn reset(&mut self) {
        self.reported = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reports_are_the_xterm_ones() {
        assert_eq!(FocusReport::In.bytes(), b"\x1b[I");
        assert_eq!(FocusReport::Out.bytes(), b"\x1b[O");
        assert!(FocusReport::In.focused() && !FocusReport::Out.focused());
    }

    #[test]
    fn nothing_is_sent_while_the_program_did_not_ask() {
        let mut reporter = FocusReporter::default();
        for focused in [true, false, true, false] {
            reporter.set_focused(focused);
            assert_eq!(reporter.poll(false), None);
        }
    }

    #[test]
    fn each_change_is_sent_once() {
        let mut reporter = FocusReporter::default();
        reporter.set_focused(true);
        assert_eq!(reporter.poll(true), None, "turned on while focused: the program already assumes it");
        reporter.set_focused(false);
        assert_eq!(reporter.poll(true), Some(FocusReport::Out));
        assert_eq!(reporter.poll(true), None, "the same state is not repeated");
        reporter.set_focused(false);
        assert_eq!(reporter.poll(true), None);
        reporter.set_focused(true);
        assert_eq!(reporter.poll(true), Some(FocusReport::In));
        assert_eq!(reporter.poll(true), None);
        // A flicker that ends where it began sends nothing between polls.
        reporter.set_focused(false);
        reporter.set_focused(true);
        assert_eq!(reporter.poll(true), None);
    }

    #[test]
    fn turning_reporting_on_without_the_focus_reports_it_at_once() {
        let mut reporter = FocusReporter::default();
        assert_eq!(reporter.poll(true), Some(FocusReport::Out), "a new terminal has no focus yet");
        assert_eq!(reporter.poll(true), None);
        reporter.set_focused(true);
        assert_eq!(reporter.poll(true), Some(FocusReport::In));
    }

    #[test]
    fn turning_reporting_off_and_on_starts_over() {
        let mut reporter = FocusReporter::default();
        reporter.set_focused(true);
        assert_eq!(reporter.poll(true), None);
        assert_eq!(reporter.poll(false), None);
        // While off the focus moves away unnoticed; on again, the program is told.
        reporter.set_focused(false);
        assert_eq!(reporter.poll(false), None);
        assert_eq!(reporter.poll(true), Some(FocusReport::Out));
        assert_eq!(reporter.poll(false), None);
        reporter.set_focused(true);
        assert_eq!(reporter.poll(true), None, "focused when it turned on again");
    }

    #[test]
    fn a_new_backend_starts_over_too() {
        let mut reporter = FocusReporter::default();
        reporter.set_focused(false);
        assert_eq!(reporter.poll(true), Some(FocusReport::Out));
        reporter.reset();
        assert_eq!(reporter.poll(true), Some(FocusReport::Out));
        reporter.set_focused(true);
        assert_eq!(reporter.poll(true), Some(FocusReport::In));
        reporter.reset();
        assert_eq!(reporter.poll(true), None);
    }
}
