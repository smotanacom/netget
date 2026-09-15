//! A panic inside `tokio::spawn` must reach the log.
//!
//! This is the check that the hook exists at all. The defect it guards against is not
//! hypothetical: three separate protocol families shipped a panicking spawned task
//! (`block_on` in `UsbInterfaceHandler::handle_urb`, `blocking_lock()` in SMB's connection
//! task, `.unwrap()` inside spawned client tasks), and in every case the task died, the server
//! stayed `Running`, the log showed the operation succeeding, and the peer hung. There was
//! nothing to grep for, which is why all three were found by reading code instead.

use std::sync::{Arc, Mutex};

use tracing::subscriber::with_default;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

/// Collects rendered events so the test can assert on what a real subscriber would have written.
#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<String>>>);

impl Collector {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

struct CollectLayer(Collector);

impl<S: tracing::Subscriber> Layer<S> for CollectLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = Visitor(String::new());
        event.record(&mut visitor);
        self.0
             .0
            .lock()
            .unwrap()
            .push(format!("{} {}", event.metadata().level(), visitor.0));
    }
}

struct Visitor(String);

impl tracing::field::Visit for Visitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={:?} ", field.name(), value));
    }
}

/// The hook logs at ERROR, names the file it panicked in, and carries the message.
///
/// The hook is installed process-wide by `init_logging`, so this test installs it directly and
/// drives a panic through `catch_unwind` — the hook runs before unwinding either way, which is
/// exactly why it sees panics `tokio::spawn` is about to swallow.
#[test]
fn a_panic_is_logged_at_error_with_its_location() {
    let collector = Collector::default();
    let subscriber = tracing_subscriber::registry().with(CollectLayer(collector.clone()));

    netget::panic_log::install();

    with_default(subscriber, || {
        let _ = std::panic::catch_unwind(|| {
            panic!("deliberate test panic");
        });
    });

    let lines = collector.lines();
    let panic_lines: Vec<_> = lines.iter().filter(|l| l.contains("PANIC")).collect();

    assert!(
        !panic_lines.is_empty(),
        "no PANIC line reached the subscriber; the hook is not installed or does not log. \
         Saw: {lines:?}"
    );
    let line = panic_lines[0];
    assert!(
        line.starts_with("ERROR"),
        "a swallowed panic must be an ERROR, not a lower level: {line}"
    );
    assert!(
        line.contains("deliberate test panic"),
        "the panic message must survive into the log: {line}"
    );
    assert!(
        line.contains("panic_is_logged_test.rs"),
        "the location must name the file that panicked, or the log says a panic happened \
         somewhere: {line}"
    );
}

/// `panic!("{}", x)` produces a `String` payload, not a `&str`.
///
/// A hook that only downcasts to `&'static str` therefore reports **every formatted panic** —
/// which is most real ones — as an unreadable payload, and the log entry becomes useless at
/// exactly the moment it matters. Both arms are asserted because only one of them is the
/// obvious one to write.
#[test]
fn a_formatted_panic_message_is_readable_not_an_opaque_payload() {
    let literal: Box<dyn std::any::Any + Send> = Box::new("static str panic");
    assert_eq!(netget::panic_log::payload_of(&*literal), "static str panic");

    let formatted: Box<dyn std::any::Any + Send> = Box::new(format!("formatted {} panic", 42));
    assert_eq!(
        netget::panic_log::payload_of(&*formatted),
        "formatted 42 panic"
    );

    // Anything else has no message to print, and saying so beats printing `Any { .. }`.
    let odd: Box<dyn std::any::Any + Send> = Box::new(7u32);
    assert_eq!(
        netget::panic_log::payload_of(&*odd),
        "<non-string panic payload>"
    );
}

/// Installing twice must not chain the hook to itself (one panic, one log line) and must not
/// drop the previous hook (the dashboard installs its terminal-restore hook over this one).
#[test]
fn installing_twice_does_not_double_log() {
    let collector = Collector::default();
    let subscriber = tracing_subscriber::registry().with(CollectLayer(collector.clone()));

    netget::panic_log::install();
    netget::panic_log::install();
    netget::panic_log::install();

    with_default(subscriber, || {
        let _ = std::panic::catch_unwind(|| panic!("once only"));
    });

    let count = collector
        .lines()
        .iter()
        .filter(|l| l.contains("once only"))
        .count();
    assert_eq!(
        count, 1,
        "install() must be idempotent; {count} log lines for one panic means each call \
         wrapped the last"
    );
}
