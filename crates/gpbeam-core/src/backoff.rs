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

/// Production jitter source (0..1000 ms). Injected into `backoff_delay` at the
/// call site so tests stay deterministic by passing a fixed value instead.
pub fn jitter_ms() -> u64 {
    use rand::Rng;
    rand::rng().random_range(0..1000)
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
}
