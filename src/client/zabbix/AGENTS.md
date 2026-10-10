# Zabbix client

The other side of the trapper server in `src/server/zabbix`, sharing its `wire` module (the
ZBXD header, 1 MiB data cap). Zabbix keeps no connection open between requests, so neither
does the client: every action is one TCP connection, one request and one answer, bounded by
`timeout_secs` (default 10). `connect` only checks the parameters and announces
`zabbix_ready {remote_addr}`.

- `zabbix_get {key}` — a passive check, as `zabbix_get` does: the plain item key, answered by
  an agent with the value or `ZBX_NOTSUPPORTED\0<reason>`. Event `zabbix_value {key, supported,
  value | error}`. A key containing a line break is refused before anything is sent.
- `zabbix_send {values: [{host, key, value, clock?}]}` — `{"request":"sender data", ...}` to a
  server or proxy trapper, as `zabbix_sender` does (numbers are sent as text, which is what
  Zabbix stores). Event `zabbix_sent {response, processed, failed, total, info}`, read from the
  trapper's own summary line, or `{response: "error", error}` when there was no answer.

At most `MAX_ITEMS` (1000) values per send; a chain stops after `MAX_FOLLOWUP_DEPTH` (8). No
TLS/PSK, no compression, no active-agent protocol.
