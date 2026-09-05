//! BitTorrent DHT protocol tests

#[cfg(all(test, feature = "torrent-dht"))]
pub mod e2e_test;

#[cfg(all(test, feature = "torrent-dht"))]
pub mod llm_failure_test;
