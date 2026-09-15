//! Write every panic to the log before the process or the task loses it.
//!
//! **A panic inside `tokio::spawn` is swallowed by the task**, and almost everything NetGet does
//! on behalf of a peer runs inside one. `JoinHandle` carries the `JoinError`, but nothing here
//! awaits those handles — a connection task is spawned and forgotten — so the panic went
//! nowhere at all. The observable result was the shape recorded three times in the root
//! `CLAUDE.md`: the task dies, the server stays `Running`, the log shows the operation
//! succeeding, and the peer hangs until its own timeout. `block_on` inside
//! `UsbInterfaceHandler::handle_urb`, `blocking_lock()` in SMB's connection task and the
//! `.unwrap()`s inside spawned client tasks were all found by reading code, because there was
//! nothing to grep for.
//!
//! The only `set_hook` in the tree before this one lives in the dashboard's event loop and
//! restores the terminal. It is installed by the TUI alone, so `--mcp`, `--mcp-http` and
//! non-interactive runs had no hook at all.
//!
//! This hook is installed from `cli::setup::init_logging`, which every entry point calls
//! before doing any work, and it **chains to whatever hook it replaced** — so the dashboard's
//! terminal-restore hook (installed later, over this one, via `take_hook`) still runs first and
//! this one still logs.

use std::sync::Once;

static INSTALLED: Once = Once::new();

/// Install the logging panic hook. Idempotent; safe to call from every entry point.
///
/// Call this *after* the tracing subscriber is initialised — a panic logged before there is a
/// subscriber goes to the same place the panic would have gone, which is nowhere.
pub fn install() {
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Build the record before touching `tracing`: if the subscriber itself is what
            // panicked, the `error!` below is lost and this is all that survives.
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "unknown location".to_string());

            // `PanicHookInfo::payload` is `&dyn Any`; the message is a `&str` for `panic!("..")`
            // and a `String` for `panic!("{}", x)`. Anything else is a payload no formatter can
            // read, and saying so is more useful than printing `Any { .. }`.
            let message = payload_of(info.payload());

            let thread = std::thread::current();
            let thread_name = thread.name().unwrap_or("<unnamed>").to_string();

            // `Backtrace::capture` honours RUST_BACKTRACE and is `Disabled` when unset, so this
            // costs nothing in the default case and is there when someone asks for it.
            let backtrace = std::backtrace::Backtrace::capture();
            let backtrace = match backtrace.status() {
                std::backtrace::BacktraceStatus::Captured => format!("\n{backtrace}"),
                _ => String::new(),
            };

            tracing::error!(
                panic.location = %location,
                panic.thread = %thread_name,
                "PANIC: {message}{backtrace}"
            );

            previous(info);
        }));
    });
}

/// Extract a panic payload's message.
///
/// Public because `tests/panic_is_logged_test.rs` asserts the `String` arm: `panic!("{x}")`
/// produces a `String` payload rather than a `&str`, and a hook that only downcasts to `&str`
/// reports every formatted panic — which is most of them — as unreadable.
pub fn payload_of(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}
