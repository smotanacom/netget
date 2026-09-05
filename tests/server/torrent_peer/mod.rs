//! BitTorrent Peer Wire protocol tests

#[cfg(all(test, feature = "torrent-peer"))]
pub mod e2e_test;

#[cfg(all(test, feature = "torrent-peer"))]
pub mod peer_inject_test;

#[cfg(all(test, feature = "torrent-peer"))]
pub mod llm_failure_test;
