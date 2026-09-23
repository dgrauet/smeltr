//! Monotonic clock based on mach_absolute_time on macOS, std::time elsewhere.

use std::time::Instant;

/// Nanoseconds on the system's raw uptime clock: `CLOCK_UPTIME_RAW`, i.e.
/// `mach_absolute_time` scaled to ns — the clock the Metal hook stamps its
/// ring frames with. 0 where no such clock exists.
pub fn uptime_raw_ns() -> u64 {
    #[cfg(target_os = "macos")]
    {
        // <time.h>: CLOCK_UPTIME_RAW = 8.
        extern "C" {
            fn clock_gettime_nsec_np(clock_id: u32) -> u64;
        }
        unsafe { clock_gettime_nsec_np(8) }
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MonoClock {
    epoch: Instant,
    /// [`uptime_raw_ns`] at `epoch`, to place raw timestamps on this clock.
    epoch_raw_ns: u64,
}

impl MonoClock {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            epoch_raw_ns: uptime_raw_ns(),
        }
    }

    /// Nanoseconds since this clock's epoch. Monotonic, never goes backward.
    pub fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    /// A [`uptime_raw_ns`] timestamp taken elsewhere (the Metal hook's ring
    /// frames) on this clock: clamped to the epoch when it predates it, and
    /// never later than now.
    pub fn at_raw_ns(&self, raw_ns: u64) -> u64 {
        raw_ns.saturating_sub(self.epoch_raw_ns).min(self.now_ns())
    }
}

impl Default for MonoClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_and_increasing() {
        let c = MonoClock::new();
        let a = c.now_ns();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = c.now_ns();
        assert!(b > a, "{b} should be > {a}");
        assert!(b - a >= 1_000_000, "elapsed should be >= 1ms in ns");
    }

    #[test]
    fn starts_near_zero() {
        let c = MonoClock::new();
        assert!(c.now_ns() < 1_000_000, "first call should be < 1ms");
    }

    /// The Metal hook stamps frames with `mach_absolute_time` scaled to ns
    /// (`smeltr_mono_ns` in ring.c); `uptime_raw_ns` must be that clock.
    #[cfg(target_os = "macos")]
    #[test]
    fn uptime_raw_is_the_hooks_mach_absolute_time() {
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        extern "C" {
            fn mach_absolute_time() -> u64;
            fn mach_timebase_info(info: *mut Timebase) -> i32;
        }
        let mut tb = Timebase { numer: 0, denom: 0 };
        let hook_ns = unsafe {
            mach_timebase_info(&mut tb);
            mach_absolute_time() * tb.numer as u64 / tb.denom as u64
        };
        let ours = uptime_raw_ns();
        assert!(ours.abs_diff(hook_ns) < 1_000_000, "{ours} vs {hook_ns}");
    }

    /// #244: an event carrying its own raw timestamp is stamped when it
    /// happened, not when the daemon got around to reading it.
    #[test]
    fn raw_timestamps_map_onto_the_session_clock() {
        let c = MonoClock::new();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let five_ms_ago = uptime_raw_ns() - 5_000_000;
        let at = c.at_raw_ns(five_ms_ago);
        let now = c.now_ns();
        assert!(
            now - at >= 5_000_000 && now - at < 6_000_000,
            "at {at}, now {now}"
        );
        // Before the session started: its origin, never a wrapped value.
        assert_eq!(c.at_raw_ns(1), 0);
        // Never in the future.
        assert!(c.at_raw_ns(u64::MAX) <= c.now_ns());
    }
}
