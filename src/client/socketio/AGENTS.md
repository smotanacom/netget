# Socket.IO client — Experimental

Opens an Engine.IO v4 session at `path` over a direct WebSocket (`transport: websocket`,
default) or HTTP long-polling (`polling`: GET to open, a reader task long-polling, POST to
send), answers server pings, and sends CONNECT for each of `namespaces` with `auth`. Events:
`socketio_connected` (namespace, socket id, transport), `socketio_connect_error`,
`socketio_event` (with `ack_id` when the server awaits an acknowledgement),
`socketio_ack_received`, `socketio_disconnected` (`*` for the whole session). Actions:
`socketio_emit` (namespace must be connected; `ack` assigns an id), `socketio_ack` (by `ack_id`),
`socketio_connect_namespace`, `socketio_disconnect_namespace`, `disconnect`. No polling-to-
WebSocket upgrade, no binary, no reconnection. Shares `src/server/socketio/packet.rs`.
