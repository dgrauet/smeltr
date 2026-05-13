//! Monotonic clock based on mach_absolute_time on macOS, std::time elsewhere.

use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct MonoClock {
    epoch: Instant,
}

impl MonoClock {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }

    /// Nanoseconds since this clock's epoch. Monotonic, never goes backward.
    pub fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
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
}
