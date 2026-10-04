# NETCONF client — Experimental, NETCONF 1.0/1.1 over SSH

Connects with russh 0.45, **requires** a pinned host key (`host_key_sha256`, OpenSSH
`SHA256:` form; a different key fails the handshake), authenticates by password, opens the
`netconf` subsystem and exchanges `<hello>`. `base_versions` (default both) chooses what is
offered; base:1.1 on both sides means chunked framing. Shares `wire`, `xml`, `rpc` and
`owned_stream` with the server (`src/server/netconf/`).

A server may start the subsystem and send its `<hello>` before CHANNEL_SUCCESS (Paramiko
does); those early bytes are kept, bounded, and fed to the decoder.

## Actions and events

- `netconf_rpc` with `operation` get, get-config, edit-config, lock, unlock, commit,
  discard-changes, validate, close-session, kill-session or custom. Rust assigns the
  `message-id` (1, 2, 3, …), builds the base-namespace envelope and refuses, before writing,
  what the server's capabilities rule out (candidate/startup, edit-config on running without
  `:writable-running`, commit/discard without `:candidate`, validate without `:validate`,
  rollback-on-error, url). `filter_xml`, `config_xml` and `input_xml` are parsed with the
  wire bounds — a DTD, entity or unbalanced markup is refused.
- `disconnect` closes the SSH connection without close-session.
- `netconf_connected` {session_id, server_capabilities, base_version};
  `netconf_rpc_reply` {operation, message_id, ok | data_xml | output_xml | errors}. A reply
  whose `message-id` differs from the request ends the session; so does any message with no
  request pending (notifications are not negotiated).

One RPC is outstanding at a time; an injected action during a pending request is
`Rejected`, except `disconnect`. `reply_timeout_secs` (60) bounds each reply and
`handshake_timeout_secs` (30) the whole connect. All tasks are registered; removing the
client cancels the SSH driver through `OwnedStream`. Injected-action access-log entries
record only operation and message-id, because configuration may carry secrets.

Not implemented: public-key authentication, notifications, NETCONF over TLS, confirmed
commit, url, copy-config/delete-config.
