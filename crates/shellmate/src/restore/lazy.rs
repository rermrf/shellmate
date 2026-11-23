//! A restored remote tab does not connect when the app starts, except the one its window shows.
//! The others wait until they are shown for the first time; a serial port is never opened by
//! itself, whatever the tab, because the device may be gone.
use crate::workspace::Panel;
use shellmate_core::Protocol;

/// What brings the connection of a restored tab up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    /// The tab becomes its window's current tab (SSH, Mosh, Telnet).
    Show,
    /// The user presses Enter in it (a serial port).
    Enter,
}

/// What happened to the tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// It became its window's current tab.
    Shown,
    /// Enter was pressed in its terminal.
    Enter,
}

/// A tab that is restored but not connected yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Deferred {
    pub wait: Wait,
    /// The panel the tab had; shown once the connection is on its way.
    pub panel: Panel,
}

/// What a tab does with an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Nothing yet.
    Wait(Deferred),
    /// Connect now; the tab is no longer deferred.
    Connect { panel: Panel },
}

impl Wait {
    /// What a restored tab of this protocol waits for.
    pub(crate) fn of(protocol: Protocol) -> Self {
        match protocol {
            Protocol::Serial => Self::Enter,
            Protocol::Ssh | Protocol::Mosh | Protocol::Telnet => Self::Show,
        }
    }

    /// What the terminal of such a tab says.
    pub(crate) fn notice(self) -> &'static str {
        match self {
            Self::Show => "切换到这个标签时连接",
            Self::Enter => "串口不会自动打开",
        }
    }

    fn starts_on(self, event: Event) -> bool {
        matches!(
            (self, event),
            (Self::Show, Event::Shown) | (Self::Enter, Event::Enter)
        )
    }
}

impl Deferred {
    pub(crate) fn on(self, event: Event) -> Step {
        if self.wait.starts_on(event) {
            Step::Connect { panel: self.panel }
        } else {
            Step::Wait(self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deferred(protocol: Protocol) -> Deferred {
        Deferred {
            wait: Wait::of(protocol),
            panel: Panel::Files,
        }
    }

    #[test]
    fn only_serial_ports_wait_for_enter() {
        assert_eq!(Wait::of(Protocol::Serial), Wait::Enter);
        for protocol in [Protocol::Ssh, Protocol::Mosh, Protocol::Telnet] {
            assert_eq!(Wait::of(protocol), Wait::Show);
        }
    }

    #[test]
    fn a_remote_tab_connects_the_first_time_it_is_shown_and_not_before() {
        for protocol in [Protocol::Ssh, Protocol::Mosh, Protocol::Telnet] {
            let tab = deferred(protocol);
            assert_eq!(
                tab.clone().on(Event::Enter),
                Step::Wait(tab.clone()),
                "Enter does nothing in a tab that is not shown"
            );
            assert_eq!(
                tab.on(Event::Shown),
                Step::Connect {
                    panel: Panel::Files
                }
            );
        }
    }

    #[test]
    fn a_serial_tab_is_never_connected_by_showing_it() {
        let tab = deferred(Protocol::Serial);
        let shown = tab.clone().on(Event::Shown);
        assert_eq!(
            shown,
            Step::Wait(tab.clone()),
            "shown again and again, still closed"
        );
        let Step::Wait(tab) = shown else {
            unreachable!()
        };
        assert_eq!(
            tab.on(Event::Enter),
            Step::Connect {
                panel: Panel::Files
            }
        );
    }

    #[test]
    fn waiting_changes_nothing_about_the_tab() {
        let tab = deferred(Protocol::Ssh);
        let mut current = tab.clone();
        for event in [Event::Enter, Event::Enter] {
            let Step::Wait(next) = current.on(event) else {
                panic!("it connected")
            };
            current = next;
        }
        assert_eq!(current, tab);
    }

    #[test]
    fn the_notices_say_what_will_happen() {
        assert_eq!(Wait::Show.notice(), "切换到这个标签时连接");
        assert!(Wait::Enter.notice().contains("串口"));
    }
}
