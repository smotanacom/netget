#[cfg(all(test, feature = "modbus"))]
mod bounds_test;
#[cfg(all(test, feature = "modbus"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "modbus"))]
mod e2e_test;
#[cfg(all(test, feature = "modbus"))]
mod llm_failure_test;
#[cfg(all(test, feature = "modbus"))]
mod pcap_oracle_test;
#[cfg(all(test, feature = "modbus"))]
mod peer_inject_test;
#[cfg(all(test, feature = "modbus"))]
mod real_client_test;
