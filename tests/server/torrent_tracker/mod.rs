//! BitTorrent Tracker protocol tests

#[cfg(all(test, feature = "torrent-tracker"))]
pub mod e2e_test;

#[cfg(all(test, feature = "torrent-tracker"))]
pub mod llm_failure_test;
#[cfg(all(test, feature = "torrent-tracker"))]
pub mod peer_inject_test;
#[cfg(all(test, feature = "torrent-tracker"))]
mod real_client_test;
