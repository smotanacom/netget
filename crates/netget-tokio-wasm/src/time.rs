//! Timers on `setTimeout`, and `Instant` on `performance.now()`.
//!
//! `std::time::Instant::now()` panics on `wasm32-unknown-unknown`. [`Instant`] here is the
//! same API over the browser clock, and NetGet's `crate::utils::clock::Instant` is this type
//! on this target, so deadlines computed anywhere in NetGet and deadlines passed to
//! `sleep_until` are one type.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use gloo_timers::future::TimeoutFuture;
use send_wrapper::SendWrapper;

pub use std::time::Duration;

/// A monotonic instant on the browser clock.
///
/// `performance.now()` starts at zero when the page loads. NetGet's rate limiter computes
/// `Instant::now() - window`, which is fine on an OS clock that counts from boot but
/// underflows on a fresh page; so this instant counts from ten years before the page loaded,
/// and behaves like `std::time::Instant` in every other respect (`Add`/`Sub` a `Duration`,
/// `Sub` two instants, `elapsed`, `duration_since` and the checked/saturating forms).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(Duration);

/// Headroom below "page load" so subtracting a window never underflows.
const INSTANT_BASE: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);

fn performance_now_ms() -> f64 {
    use wasm_bindgen::JsCast;
    let global = js_sys::global();
    if let Ok(perf) = js_sys::Reflect::get(&global, &wasm_bindgen::JsValue::from_str("performance"))
    {
        if let Ok(now) = js_sys::Reflect::get(&perf, &wasm_bindgen::JsValue::from_str("now")) {
            if let Ok(f) = now.dyn_into::<js_sys::Function>() {
                if let Some(ms) = f.call0(&perf).ok().and_then(|v| v.as_f64()) {
                    return ms;
                }
            }
        }
    }
    js_sys::Date::now()
}

impl Instant {
    pub fn now() -> Instant {
        Instant(INSTANT_BASE + Duration::from_secs_f64(performance_now_ms().max(0.0) / 1000.0))
    }

    pub fn duration_since(&self, earlier: Instant) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
        self.0.checked_sub(earlier.0)
    }

    pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    pub fn elapsed(&self) -> Duration {
        Instant::now().duration_since(*self)
    }

    pub fn checked_add(&self, duration: Duration) -> Option<Instant> {
        self.0.checked_add(duration).map(Instant)
    }

    pub fn checked_sub(&self, duration: Duration) -> Option<Instant> {
        self.0.checked_sub(duration).map(Instant)
    }
}

impl std::ops::Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, rhs: Duration) -> Instant {
        self.checked_add(rhs)
            .expect("overflow when adding duration to instant")
    }
}

impl std::ops::AddAssign<Duration> for Instant {
    fn add_assign(&mut self, rhs: Duration) {
        *self = *self + rhs;
    }
}

impl std::ops::Sub<Duration> for Instant {
    type Output = Instant;
    fn sub(self, rhs: Duration) -> Instant {
        self.checked_sub(rhs)
            .expect("overflow when subtracting duration from instant")
    }
}

impl std::ops::SubAssign<Duration> for Instant {
    fn sub_assign(&mut self, rhs: Duration) {
        *self = *self - rhs;
    }
}

impl std::ops::Sub<Instant> for Instant {
    type Output = Duration;
    fn sub(self, rhs: Instant) -> Duration {
        self.duration_since(rhs)
    }
}

impl fmt::Debug for Instant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Instant").field(&self.0).finish()
    }
}

pub mod error {
    use std::fmt;

    /// The deadline of a [`super::timeout`] passed before the future completed.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Elapsed(pub(super) ());

    impl fmt::Display for Elapsed {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("deadline has elapsed")
        }
    }

    impl std::error::Error for Elapsed {}

    impl From<Elapsed> for std::io::Error {
        fn from(_: Elapsed) -> Self {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "deadline has elapsed")
        }
    }
}

/// `setTimeout` takes a 32-bit millisecond delay; anything longer is clamped. 49 days is
/// far past any page's lifetime.
const MAX_DELAY_MS: u64 = u32::MAX as u64;

fn clamp_delay(d: Duration) -> u32 {
    d.as_millis().min(MAX_DELAY_MS as u128) as u32
}

/// A future that completes at a deadline.
pub struct Sleep {
    deadline: Instant,
    // Armed lazily on first poll, so a `Sleep` created and never awaited costs nothing. The
    // `TimeoutFuture` holds a JS closure and is `!Send`; `SendWrapper` makes the `Sleep`
    // (and every future that embeds one) `Send` for the type checker, which is sound on a
    // single-threaded target.
    timer: Option<SendWrapper<Pin<Box<TimeoutFuture>>>>,
}

impl Sleep {
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn is_elapsed(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn reset(self: Pin<&mut Self>, deadline: Instant) {
        let this = self.get_mut();
        this.deadline = deadline;
        this.timer = None;
    }
}

impl Unpin for Sleep {}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            let now = Instant::now();
            if now >= self.deadline {
                self.timer = None;
                return Poll::Ready(());
            }
            if self.timer.is_none() {
                let ms = clamp_delay(self.deadline.saturating_duration_since(now));
                self.timer = Some(SendWrapper::new(Box::pin(TimeoutFuture::new(ms))));
            }
            let timer = self.timer.as_mut().expect("armed above");
            match Pin::new(&mut **timer).poll(cx) {
                Poll::Pending => return Poll::Pending,
                // Fired: loop back to re-check the clock. A `setTimeout` can fire a hair
                // early; if so a fresh timer covers the remainder.
                Poll::Ready(()) => self.timer = None,
            }
        }
    }
}

impl fmt::Debug for Sleep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sleep")
            .field("deadline", &self.deadline)
            .finish()
    }
}

pub fn sleep(duration: Duration) -> Sleep {
    sleep_until(deadline_after(duration))
}

pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep {
        deadline,
        timer: None,
    }
}

fn deadline_after(duration: Duration) -> Instant {
    let bounded = Duration::from_millis(clamp_delay(duration) as u64);
    Instant::now()
        .checked_add(bounded)
        .unwrap_or_else(Instant::now)
}

pin_project_lite::pin_project! {
    /// A future bounded by a deadline; see [`timeout`].
    pub struct Timeout<F> {
        #[pin]
        future: F,
        delay: Sleep,
    }
}

impl<F> Timeout<F> {
    pub fn get_ref(&self) -> &F {
        &self.future
    }

    pub fn get_mut(&mut self) -> &mut F {
        &mut self.future
    }

    pub fn into_inner(self) -> F {
        self.future
    }
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, error::Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Poll::Ready(v) = this.future.poll(cx) {
            return Poll::Ready(Ok(v));
        }
        match Pin::new(this.delay).poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(error::Elapsed(()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Require `future` to complete within `duration`.
pub fn timeout<F: Future>(duration: Duration, future: F) -> Timeout<F> {
    Timeout {
        future,
        delay: sleep(duration),
    }
}

/// Require `future` to complete before `deadline`.
pub fn timeout_at<F: Future>(deadline: Instant, future: F) -> Timeout<F> {
    Timeout {
        future,
        delay: sleep_until(deadline),
    }
}

/// What an [`Interval`] does when a tick was missed because `tick` was not awaited in time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MissedTickBehavior {
    /// Fire the missed ticks back to back, then resume the schedule.
    #[default]
    Burst,
    /// Fire once now and restart the schedule from now.
    Delay,
    /// Drop the missed ticks and resume the original schedule.
    Skip,
}

/// Fires at a fixed period. The first tick completes immediately, as tokio's does.
#[derive(Debug)]
pub struct Interval {
    period: Duration,
    next: Instant,
    behavior: MissedTickBehavior,
}

impl Interval {
    pub async fn tick(&mut self) -> Instant {
        sleep_until(self.next).await;
        let fired = self.next;
        let now = Instant::now();
        self.next = match self.behavior {
            MissedTickBehavior::Burst => fired + self.period,
            MissedTickBehavior::Delay => now + self.period,
            MissedTickBehavior::Skip => {
                let mut next = fired + self.period;
                while next <= now {
                    next += self.period;
                }
                next
            }
        };
        fired
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.behavior
    }

    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.behavior = behavior;
    }

    pub fn reset(&mut self) {
        self.next = Instant::now() + self.period;
    }

    pub fn reset_immediately(&mut self) {
        self.next = Instant::now();
    }

    pub fn reset_at(&mut self, deadline: Instant) {
        self.next = deadline;
    }
}

pub fn interval(period: Duration) -> Interval {
    assert!(period > Duration::ZERO, "`period` must be non-zero.");
    interval_at(Instant::now(), period)
}

pub fn interval_at(start: Instant, period: Duration) -> Interval {
    assert!(period > Duration::ZERO, "`period` must be non-zero.");
    Interval {
        period,
        next: start,
        behavior: MissedTickBehavior::default(),
    }
}
