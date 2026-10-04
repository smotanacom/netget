//! CalDAV client: the shared DAV client engine in `server::dav_common::client` with CalDAV's flavor.
pub mod actions;
/// The engine raises `actions::CONNECTED_EVENT` after discovery and `actions::RESPONSE_EVENT` after
/// each operation, through this flavor.
pub static FLAVOR: crate::server::dav_common::client::ClientFlavor =
    crate::server::dav_common::client::ClientFlavor {
        kind: crate::server::dav_common::server::Kind::Calendar,
        name: "CalDAV",
        prefix: "caldav",
        connected_event: &actions::CONNECTED_EVENT,
        response_event: &actions::RESPONSE_EVENT,
        protocol: &actions::PROTOCOL,
    };
