//! ESP32 auto-program circuit sequences. serialport `true` means asserted,
//! which is a LOW physical voltage on active-low USB modem pins. DTR controls
//! GPIO0, RTS controls EN through the standard ESP32 auto-reset transistor pair.
//! RESET keeps DTR physically HIGH (deasserted/false) while pulsing EN with RTS.
//! DOWNLOAD is esptool's classic sequence: GPIO0 released + EN low, GPIO0 low +
//! EN released, then GPIO0 released. Boards with other wiring need manual boot.
use std::time::Duration;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Dtr(bool),
    Rts(bool),
    Wait(Duration),
}
pub const RESET: &[Step] = &[
    Step::Dtr(false),
    Step::Rts(true),
    Step::Wait(Duration::from_millis(100)),
    Step::Rts(false),
];
pub const DOWNLOAD: &[Step] = &[
    Step::Dtr(false),
    Step::Rts(true),
    Step::Wait(Duration::from_millis(100)),
    Step::Dtr(true),
    Step::Rts(false),
    Step::Wait(Duration::from_millis(50)),
    Step::Dtr(false),
];
pub trait Pins {
    type Error;
    fn dtr(&mut self, on: bool) -> Result<(), Self::Error>;
    fn rts(&mut self, on: bool) -> Result<(), Self::Error>;
    fn wait(&mut self, d: Duration);
}
pub fn run<P: Pins>(pins: &mut P, steps: &[Step]) -> Result<(), P::Error> {
    for step in steps {
        match *step {
            Step::Dtr(on) => pins.dtr(on)?,
            Step::Rts(on) => pins.rts(on)?,
            Step::Wait(d) => pins.wait(d),
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Trace(Vec<Step>);
    impl Pins for Trace {
        type Error = ();
        fn dtr(&mut self, on: bool) -> Result<(), ()> {
            self.0.push(Step::Dtr(on));
            Ok(())
        }
        fn rts(&mut self, on: bool) -> Result<(), ()> {
            self.0.push(Step::Rts(on));
            Ok(())
        }
        fn wait(&mut self, d: Duration) {
            self.0.push(Step::Wait(d));
        }
    }
    #[test]
    fn exact_esptool_sequence_and_reset() {
        let mut t = Trace(Vec::new());
        run(&mut t, DOWNLOAD).unwrap();
        assert_eq!(
            t.0,
            vec![
                Step::Dtr(false),
                Step::Rts(true),
                Step::Wait(Duration::from_millis(100)),
                Step::Dtr(true),
                Step::Rts(false),
                Step::Wait(Duration::from_millis(50)),
                Step::Dtr(false)
            ]
        );
        t.0.clear();
        run(&mut t, RESET).unwrap();
        assert_eq!(t.0, RESET);
        assert!(!t.0.contains(&Step::Dtr(true)));
    }
}
