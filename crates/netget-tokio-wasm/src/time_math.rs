//! Pure timer arithmetic, shared with host-side regression tests.

use std::time::Duration;

/// gloo-timers casts the delay to i32 before calling JavaScript's setTimeout.
/// Round upward so a positive sub-millisecond remainder never becomes a zero timer.
pub(super) fn timer_delay_ms(duration: Duration) -> u32 {
    duration
        .as_nanos()
        .div_ceil(1_000_000)
        .min(i32::MAX as u128) as u32
}

/// Delay until the next tick on the original schedule, in constant time even after
/// a suspended page misses millions of ticks.
pub(super) fn skip_delay(elapsed: Duration, period: Duration) -> Duration {
    let nanos = period.as_nanos() - elapsed.as_nanos() % period.as_nanos();
    Duration::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    )
}
