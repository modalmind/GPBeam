//! Emit-rate throttle for live transfer progress. Each `RunEvent::Progress`
//! folds into the GUI snapshot and is broadcast to every window, so emitting one
//! per network/disk chunk would flood the channel on a multi-GB file. This gates
//! emission to ~100 updates per file: one per integer-percent step of the current
//! file, plus a guaranteed final tick at completion.

/// Tracks the last emitted integer-percent for one file's download/copy and
/// decides whether a new cumulative byte count is worth emitting.
#[derive(Debug)]
pub struct ProgressThrottle {
    total: u64,
    /// Last emitted integer-percent; `-1` is the sentinel for "nothing emitted yet"
    /// (hence `i64` rather than `u8`).
    last_pct: i64,
}

impl ProgressThrottle {
    /// `total` is the file's expected size. A zero total (unknown/empty file)
    /// emits once on the first observation.
    pub fn new(total: u64) -> Self {
        Self {
            total,
            last_pct: -1,
        }
    }

    /// Returns true when `cum` (cumulative bytes for this file) should be emitted:
    /// when its integer-percent of `total` has advanced, or it has reached `total`
    /// (the terminal tick). Each distinct percent emits at most once. Assumes `cum`
    /// is monotonically non-decreasing; feeding it a smaller value may re-emit a
    /// percent bucket.
    #[must_use]
    pub fn should_emit(&mut self, cum: u64) -> bool {
        let pct: i64 = if self.total == 0 || cum >= self.total {
            100
        } else {
            (cum.saturating_mul(100) / self.total) as i64
        };
        if pct != self.last_pct {
            self.last_pct = pct;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_on_each_percent_step() {
        let mut t = ProgressThrottle::new(1000);
        assert!(t.should_emit(0), "first observation (0%) emits");
        assert!(!t.should_emit(5), "still 0%");
        assert!(t.should_emit(10), "1% -> emit");
        assert!(!t.should_emit(15), "still 1%");
        assert!(t.should_emit(20), "2% -> emit");
    }

    #[test]
    fn always_emits_terminal_tick() {
        let mut t = ProgressThrottle::new(1000);
        assert!(t.should_emit(990), "99%");
        assert!(t.should_emit(1000), "reaching total emits (100%)");
        assert!(!t.should_emit(1000), "100% only emits once");
        assert!(!t.should_emit(1001), "overshoot does not re-emit");
    }

    #[test]
    fn zero_total_emits_once() {
        let mut t = ProgressThrottle::new(0);
        assert!(t.should_emit(0), "unknown/empty size emits on first call");
        assert!(!t.should_emit(0), "no repeat");
    }

    #[test]
    fn caps_emissions_to_about_one_hundred() {
        let mut t = ProgressThrottle::new(10_000_000);
        let mut emits = 0;
        for i in (0..=10_000_000u64).step_by(1000) {
            if t.should_emit(i) {
                emits += 1;
            }
        }
        // 0%..100% inclusive == 101 distinct percent buckets.
        assert!((99..=101).contains(&emits), "got {emits}");
    }
}
