//! Host-only timer arithmetic checks: no browser, runtime, GPU, or waiting.
#[path = "../src/time_math.rs"]
mod time_math;

use std::time::Duration;

#[test]
fn host_timer_chunks_stay_within_the_signed_limit() {
    for duration in [Duration::from_secs(30 * 86400), Duration::MAX] {
        assert_eq!(time_math::timer_delay_ms(duration), i32::MAX as u32);
    }
}

#[test]
fn fractional_milliseconds_are_rounded_up() {
    assert_eq!(time_math::timer_delay_ms(Duration::ZERO), 0);
    assert_eq!(time_math::timer_delay_ms(Duration::from_nanos(1)), 1);
    assert_eq!(time_math::timer_delay_ms(Duration::from_micros(1001)), 2);
    assert_eq!(time_math::timer_delay_ms(Duration::from_millis(7)), 7);
}

#[test]
fn missed_ticks_skip_directly_to_the_next_original_boundary() {
    let period = Duration::from_millis(100);
    assert_eq!(time_math::skip_delay(Duration::ZERO, period), period);
    assert_eq!(
        time_math::skip_delay(Duration::from_millis(250), period),
        Duration::from_millis(50)
    );
    assert_eq!(
        time_math::skip_delay(Duration::from_millis(300), period),
        period
    );
    assert_eq!(
        time_math::skip_delay(Duration::from_secs(86400 * 365), Duration::from_nanos(1)),
        Duration::from_nanos(1)
    );
}
