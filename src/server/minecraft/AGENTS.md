# Minecraft server (Java Edition server-list ping and login refusal)

Hand-written over Tokio TCP (`wire.rs`, shared with the client). No game is run: NetGet
answers what a launcher's server list and a joining client see first, and nothing after.

## What Rust owns

VarInt framing and every bound; the handshake (protocol, address, port, next state 1, 2 or
3); the status JSON built from the handler's fields; the ping/pong (the payload is echoed and
the connection closed, as vanilla does); and the pre-1.7 legacy ping in all three forms —
bare `FE` (Beta 1.8–1.3, answered `motd§online§max`), `FE 01` (1.4–1.5) and `FE 01 FA` +
MC|PingHost (1.6), the last two answered `§1\0protocol\0version\0motd\0online\0max`. Each
optional legacy tail is waited for 300 ms, because older clients send nothing more;
mcstatus sends `FE 01 FA` and no MC|PingHost at all. Login Start is read by shape (name,
then a 16-byte UUID, an optional one, or nothing) rather than by version number.

## What the handler decides

- `minecraft_status_request {protocol_version, server_address, server_port, legacy,
  remote_addr}` → `minecraft_status {motd, online_players, max_players, version_name?,
  protocol?, sample?, enforces_secure_chat?}` or `minecraft_refuse`. `protocol` defaults to
  767 with `version_name` 1.21.1; `"echo"` repeats the client's. A sample entry without an
  `id` gets the nil UUID.
- `minecraft_login {username, uuid, protocol_version, server_address, server_port, transfer,
  remote_addr}` → `minecraft_disconnect {reason}` or `minecraft_refuse`.

No favicon: it is base64 image data, which the action rules forbid.

## Failure modes and bounds

A ping the handler fails, refuses or leaves unanswered is closed without a status, because a
status is an assertion about the server. A login the handler fails on is disconnected with
`WireFailure`'s generic text, because a disconnect is a refusal and invents nothing; a
`minecraft_refuse` closes it unanswered. Each outcome logs its `decision=` tag. Serverbound
packets are capped at 2048 bytes before they are read, the handshake address at 255
characters, a player name at 16, the status JSON and a reason at 32767; one status request
per connection; each read waits `idle_timeout_secs` (default 30, at most 300).
