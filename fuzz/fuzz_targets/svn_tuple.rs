//! `netget::server::svn::wire` — ra_svn item framing, and the event builder that walks what it
//! parsed.
//!
//! The reader is iterative (an explicit stack of open lists), so reading a deep tuple cannot
//! overflow the stack. What it produces cannot say the same: `Item` is a recursive enum, and
//! everything downstream of the reader walks it recursively — `Display` (`command_line`),
//! `to_json` (`args`), and `Drop`. A peer can close every list it opens, so a 100,000-deep tuple
//! costs 200 KB on the wire and, without `MAX_TUPLE_DEPTH`, becomes a 100,000-deep `Item` whose
//! first recursive walk takes the process down. The depth bound is what protects those walkers,
//! and this target drives them: every item `read_item` accepts goes to `command_event_data`, the
//! function the session calls, and is then dropped.
//!
//! The corpus carries `depth_bomb` — 100,000 nested, *closed* lists (an unclosed one never
//! becomes an `Item`) — and `at_depth_limit`, exactly `MAX_TUPLE_DEPTH` deep.
//!
//! `read_item` is async over any `AsyncRead`. A byte slice never returns `Pending`, so the
//! future is polled to completion with a no-op waker rather than pulling in a runtime.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::svn::command_event_data;
use netget::server::svn::wire::ItemReader;
use netget::server::svn::MAX_COMMAND_BYTES;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1_000_000 {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
    }
    panic!("read_item returned Pending on an in-memory slice");
}

fuzz_target!(|data: &[u8]| {
    let mut reader = ItemReader::new(data);
    let mut total: u64 = 0;
    // Bounded: a reader that consumed nothing and returned an item would otherwise loop.
    for _ in 0..64 {
        match block_on(reader.read_item(MAX_COMMAND_BYTES)) {
            Ok(Some((item, consumed))) => {
                assert!(consumed > 0 && consumed <= MAX_COMMAND_BYTES);
                total += consumed;
                assert!(
                    total <= data.len() as u64,
                    "read_item reported {total} bytes consumed from a {}-byte input",
                    data.len()
                );
                let (command, event) = command_event_data(&item, "127.0.0.1");
                assert_eq!(event["command"].as_str(), Some(command.as_str()));
                // Explicit, because the drop is one of the three recursive walks.
                drop(item);
            }
            Ok(None) | Err(_) => break,
        }
    }
});
