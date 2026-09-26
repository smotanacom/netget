#[cfg(all(test, feature = "zabbix"))]
mod common;
#[cfg(all(test, feature = "zabbix"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "zabbix"))]
mod e2e_test;
#[cfg(all(test, feature = "zabbix"))]
mod llm_failure_test;
#[cfg(all(test, feature = "zabbix"))]
mod peer_inject_test;
#[cfg(all(test, feature = "zabbix"))]
mod real_client_test;
#[cfg(all(test, feature = "zabbix"))]
mod wire_test;
