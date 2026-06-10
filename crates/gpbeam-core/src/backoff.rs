use std::time::Duration;

/// Hard ceiling for the backoff delay.
const MAX_BACKOFF_SECS: u64 = 64;

/// Truncated exponential backoff. The base delay for `attempts` is
/// `2^attempts` seconds, plus `jitter_ms` milliseconds, capped at 64 seconds.
/// `attempts` is the number of attempts already made (1 = after the first
/// failure). Saturating math keeps large `attempts` from overflowing.
pub fn backoff_delay(attempts: u32, jitter_ms: u64) -> Duration {
    // 2^attempts seconds, capped before adding jitter; checked_pow guards overflow.
    let base_secs = 2u64
        .checked_pow(attempts)
        .unwrap_or(u64::MAX)
        .min(MAX_BACKOFF_SECS);
    let base = Duration::from_secs(base_secs);
    let total = base.saturating_add(Duration::from_millis(jitter_ms));
    // Final clamp so jitter can never push past the ceiling.
    total.min(Duration::from_secs(MAX_BACKOFF_SECS))
}

/// Whole-second view of `backoff_delay` for callers persisting a Unix
/// `next_retry_at`: rounds sub-second jitter UP instead of flooring it away
/// (a plain `as_secs()` on a whole-second base + 0..1000ms jitter would strip
/// the jitter entirely, defeating its anti-thundering-herd purpose).
pub fn backoff_delay_secs(attempts: u32, jitter_ms: u64) -> i64 {
    backoff_delay(attempts, jitter_ms)
        .as_millis()
        .div_ceil(1000) as i64
}

/// Production jitter source (0..4000 ms). Injected into `backoff_delay` at the
/// call site so tests stay deterministic by passing a fixed value instead.
///
/// The range spans several whole seconds on purpose: persisted retry schedules
/// are whole-second (`backoff_delay_secs` rounds up), so a sub-second range
/// would collapse to a constant +1s for 99.9% of draws and the
/// anti-thundering-herd spread would vanish.
pub fn jitter_ms() -> u64 {
    use rand::Rng;
    rand::rng().random_range(0..4000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_one_is_about_two_seconds_plus_jitter() {
        // 2^1 = 2s base, no jitter.
        assert_eq!(backoff_delay(1, 0), Duration::from_secs(2));
    }

    #[test]
    fn jitter_is_added_to_the_base() {
        // 2^1 = 2s base + 250ms jitter = 2250ms.
        assert_eq!(backoff_delay(1, 250), Duration::from_millis(2_250));
    }

    #[test]
    fn attempt_two_is_four_seconds() {
        assert_eq!(backoff_delay(2, 0), Duration::from_secs(4));
    }

    #[test]
    fn large_attempts_are_capped_at_sixty_four_seconds() {
        // 2^10 = 1024s uncapped; must clamp to 64s. Jitter must not push past the cap.
        assert_eq!(backoff_delay(10, 0), Duration::from_secs(64));
        assert_eq!(backoff_delay(10, 5_000), Duration::from_secs(64));
        assert_eq!(backoff_delay(100, 0), Duration::from_secs(64)); // no overflow panic
    }

    #[test]
    fn attempt_zero_is_one_second() {
        // 2^0 = 1s — defensive lower bound.
        assert_eq!(backoff_delay(0, 0), Duration::from_secs(1));
    }

    #[test]
    fn whole_second_schedule_keeps_jitter_alive() {
        // The whole-second view must round sub-second jitter UP, never floor
        // it away: schedules computed with different jitter draws can differ,
        // keeping the anti-thundering-herd jitter effective.
        assert_eq!(backoff_delay_secs(1, 0), 2);
        assert_eq!(backoff_delay_secs(1, 1), 3);
        assert_eq!(backoff_delay_secs(1, 999), 3);
        assert_ne!(backoff_delay_secs(1, 0), backoff_delay_secs(1, 500));
    }

    #[test]
    fn whole_second_schedule_respects_the_cap() {
        // Jitter rounding must not push past the 64s ceiling.
        assert_eq!(backoff_delay_secs(10, 0), 64);
        assert_eq!(backoff_delay_secs(10, 999), 64);
        assert_eq!(backoff_delay_secs(100, 999), 64); // no overflow panic
    }
}
