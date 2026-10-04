//! WebDAV machinery CalDAV and CardDAV share: object parsing (`object`), XML (`xml`), the
//! server engine (`server`), the client engine (`client`) and the per-protocol actions.
pub mod actions;
pub mod client;
pub mod object;
pub mod server;
pub mod xml;
