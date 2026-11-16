use std::time::Duration;
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
}
/// On macOS, continuous time includes sleep. An idle vault must not stay
/// unlocked after the laptop has slept for longer than the lock interval.
#[cfg_attr(target_os = "macos", derive(Default))]
pub struct MonotonicClock {
    #[cfg(not(target_os = "macos"))]
    started: std::time::Instant,
}
#[cfg(not(target_os = "macos"))]
impl Default for MonotonicClock {
    fn default() -> Self {
        Self {
            #[cfg(not(target_os = "macos"))]
            started: std::time::Instant::now(),
        }
    }
}
impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        #[cfg(target_os = "macos")]
        {
            #[repr(C)]
            struct Timebase {
                numer: u32,
                denom: u32,
            }
            unsafe extern "C" {
                fn mach_continuous_time() -> u64;
                fn mach_timebase_info(info: *mut Timebase) -> i32;
            }
            static SCALE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
            let &(numer, denom) = SCALE.get_or_init(|| {
                let mut info = Timebase { numer: 1, denom: 1 };
                // SAFETY: a writable repr(C) buffer with the system's exact ABI.
                let status = unsafe { mach_timebase_info(&mut info) };
                if status == 0 && info.denom != 0 {
                    (info.numer, info.denom)
                } else {
                    (1, 1)
                }
            });
            // SAFETY: side-effect-free system monotonic clock, available on macOS 10.12+.
            let ticks = unsafe { mach_continuous_time() };
            Duration::from_nanos(
                (u128::from(ticks) * u128::from(numer) / u128::from(denom)).min(u64::MAX as u128)
                    as u64,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.started.elapsed()
        }
    }
}
pub struct IdleLock {
    clock: Box<dyn Clock>,
    last: Duration,
    pub enabled: bool,
}
impl IdleLock {
    pub fn new(clock: Box<dyn Clock>) -> Self {
        let last = clock.now();
        Self {
            clock,
            last,
            enabled: true,
        }
    }
    pub fn activity(&mut self) {
        self.last = self.clock.now();
    }
    pub fn expired(&self) -> bool {
        self.enabled && self.clock.now().saturating_sub(self.last) >= Duration::from_secs(15 * 60)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    struct Fake(Arc<AtomicU64>);
    impl Clock for Fake {
        fn now(&self) -> Duration {
            Duration::from_secs(self.0.load(Ordering::SeqCst))
        }
    }
    #[test]
    fn input_resets_deadline() {
        let t = Arc::new(AtomicU64::new(0));
        let mut idle = IdleLock::new(Box::new(Fake(t.clone())));
        t.store(899, Ordering::SeqCst);
        assert!(!idle.expired());
        idle.activity();
        t.store(900, Ordering::SeqCst);
        assert!(!idle.expired());
        t.store(1799, Ordering::SeqCst);
        assert!(idle.expired());
        idle.enabled = false;
        assert!(!idle.expired());
    }
}
