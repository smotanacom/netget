//! The monotonic and wall clocks, on every target NetGet builds for.
//!
//! `std::time::Instant::now()` and `SystemTime::now()` **panic** on `wasm32-unknown-unknown`
//! ("time not implemented on this platform"); the browser has clocks, std just does not know
//! how to reach them. On that target `Instant` is the tokio shim's `performance.now()` clock
//! and `SystemTime` is `web_time`'s `Date.now()`; on native both are std, so on native this
//! module changes nothing but a path.
//!
//! Use `crate::utils::clock::{Instant, SystemTime, UNIX_EPOCH}` rather than `std::time::*` in
//! anything the browser build compiles: `src/state`, `src/llm`, `src/protocol`, `src/cli`,
//! `src/tui`, the shared server modules and the protocols enabled in `crates/netget-web`. A
//! protocol that is only ever built natively can keep using std; the moment it is added to the
//! web build, every `Instant::now()` it reaches is a panic on first connection, so run the
//! rewrite in that direction rather than trusting it. `Duration` has no clock in it and stays
//! `std::time::Duration` everywhere.

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::{Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

// The monotonic clock is the tokio shim's (`crates/netget-tokio-wasm/src/time.rs`): the same
// type `tokio::time::sleep_until` takes, and offset so `Instant::now() - window` cannot
// underflow on a page that just loaded. The wall clock is `Date.now()` through `web-time`.
#[cfg(target_arch = "wasm32")]
pub use tokio::time::Instant;
#[cfg(target_arch = "wasm32")]
pub use web_time::{SystemTime, SystemTimeError, UNIX_EPOCH};

/// The current process id, or `0` where there is no process (the browser).
///
/// `std::process::id()` panics on `wasm32-unknown-unknown`; it is used here only to make
/// identifiers unique across NetGet processes, and there is one page per wasm instance.
pub fn process_id() -> u32 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::process::id()
    }
    #[cfg(target_arch = "wasm32")]
    {
        0
    }
}

/// A wall-clock time as a local `chrono` date-time.
///
/// `chrono` converts `std::time::SystemTime`; on wasm ours is `web_time`'s, so the conversion
/// goes through the epoch offset, which is target-neutral arithmetic.
pub fn to_local_datetime(t: SystemTime) -> chrono::DateTime<chrono::Local> {
    let since_epoch = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    chrono::DateTime::<chrono::Local>::from(std::time::UNIX_EPOCH + since_epoch)
}
