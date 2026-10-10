# Zabbix client tests

Zabbix 7.0's own daemons, from the `zabbix-agent` and `zabbix-proxy-sqlite3` packages on
repo.zabbix.com (`NETGET_ZABBIX_AGENTD` / `NETGET_ZABBIX_PROXY`, default `/usr/sbin/...`). The
tests fail without them. Each daemon runs in the foreground with a config in a temp dir, on a
port from 20000-32767 (Zabbix refuses a `ListenPort` above 32767), in a process group of its
own that is killed whole on drop — both fork workers that outlive a killed parent.

- `netget_queries_zabbix_agentd`: a chain asks `agent.ping` (`1`), then `agent.hostname` (the
  configured `netget-agent`), then an unknown key (`Unsupported item key.`); a key with a line
  break is refused locally.
- `netget_sends_to_zabbix_proxy`: two values to a proxy with no server configuration. It
  answers `processed: 0; failed: 2; total: 2`, and its DebugLevel 4 log shows the exact request
  it parsed and the host and key it then looked up.

Mutation-checked: dropping the model's actions fails both. No LLM calls.
