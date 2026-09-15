//! `tokio` for the browser, as far as NetGet needs it.
//!
//! NetGet's protocol servers are written against `tokio::net`, `tokio::spawn` and
//! `tokio::time`. None of those exist on `wasm32-unknown-unknown`: there are no sockets, no
//! threads and no clock the tokio runtime could drive. Rather than sprinkle `#[cfg]` over
//! several hundred call sites, the `netget` crate depends on this crate **under the name
//! `tokio`** on that target (`[target.'cfg(target_arch = "wasm32")'.dependencies]` in the root
//! `Cargo.toml`), so `tokio::spawn(..)`, `tokio::time::timeout(..)` and
//! `tokio::net::TcpListener::bind(..)` in protocol code resolve here unchanged.
//!
//! What is real tokio, re-exported: [`sync`], [`io`], [`select!`], [`join!`], [`pin!`]. Those
//! are runtime-independent and compile for wasm as they are. Everything else is this crate's:
//!
//! - [`spawn`] / [`task`] run futures on the JS event loop (`wasm_bindgen_futures`), with a
//!   `JoinHandle` that can be awaited and aborted the way tokio's can.
//! - [`time`] puts `sleep`, `timeout` and `interval` on `setTimeout`, and `Instant` on
//!   `performance.now()`, offset so that subtracting a window from "now" on a fresh page
//!   does not underflow.
//! - [`net`] is a **virtual network**: `TcpListener::bind` registers a port in a process-wide
//!   table and `TcpStream::connect` to that port hands the listener one end of an in-memory
//!   duplex. Nothing leaves the page. The browser demo's Telnet client and fake browser
//!   connect through it.
//! - [`process`], [`fs`], [`signal`] and [`runtime`] exist so the code that names them
//!   compiles; every operation reports `Unsupported`.
//!
//! Nothing here should be reached from native builds: the root `Cargo.toml` selects real
//! tokio there.

pub use tokio::{io, join, pin, select, sync, try_join};

pub mod fs;
pub mod net;
pub mod process;
pub mod runtime;
pub mod signal;
pub mod task;
pub mod time;

pub use task::spawn;
