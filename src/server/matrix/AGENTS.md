# Matrix server (a homeserver)

The Matrix client-server API v3 over HTTP/1.1 (hyper), well-known port 8008. One homeserver,
no federation. NetGet holds the transport state a homeserver needs to hand one client's events
to another (`hub.rs`); everything a room *says* comes from the clients or the model.

## What Rust owns

- `/_matrix/client/versions`, `GET`/`POST login` (`m.login.password` only), `logout`,
  `account/whoami`.
- Access tokens (`ngt_…`, at most `MAX_TOKENS`), from the `Authorization: Bearer` header or the
  `access_token` query parameter. Missing → 401 `M_MISSING_TOKEN`, unknown → 401
  `M_UNKNOWN_TOKEN`.
- Rooms (`!<18 letters>:<server_name>`, at most `MAX_ROOMS`): members, invitees, and the
  last `ROOM_HISTORY` events. Creating a room posts `m.room.create`, the creator's join,
  `m.room.join_rules` and `m.room.name`, then one invite per invitee.
- `/sync`: one queue per user (at most `MAX_QUEUE`; past it the oldest go and the next sync
  says `limited`). An item stays queued until the client asks for a later batch, so a lost
  response is delivered again. `since` is `s<seq>`. A sync with nothing new waits on a
  `Notify` for its `timeout`, capped at `MAX_SYNC_WAIT` (30 s), then answers the same batch.
  Invites go in `rooms.invite` with stripped state (membership, join rules, name).
- Joining hands the joiner the room's history, then everyone sees the join.
- `rooms/{id}/send/{type}/{txn}`: the same (user, txn) answers the same event id without
  asking the model again (the last `MAX_TXNS`).
- `rooms/{id}/messages` (newest first unless `dir=f`, at most `MAX_PAGE`), `joined_rooms`,
  `rooms/{id}/joined_members`, `rooms/{id}/invite`, `rooms/{id}/leave`, user filters (accepted,
  always id `0`). Everything else is 404 `M_UNRECOGNIZED`.
- Logins, when `user_passwords` is given: Rust compares the password and the model is not
  asked. The name is chosen so `utils::redact` masks it wherever startup parameters are printed.

## What the model decides

Four events, each offering `matrix_accept`, `matrix_reject{errcode, error}` and (except login)
`matrix_send{room_id?, body | content, msgtype?, event_type?}`:

- `matrix_login {user_id, device_id}`: only without `user_passwords`. The password is never
  shown.
- `matrix_create_room {user_id, name, invite}`
- `matrix_join {user_id, room_id, room_name, invited}`: unknown rooms are 404 before asking.
- `matrix_room_message {room_id, room_name, sender, event_type, content, members}`: a non-member
  is 403 before asking. The event is posted only after the model accepts.

`matrix_send` implies acceptance. It posts as the bot user (`bot_user`, default `netget`), who
joins the room first if needed; `room_id` defaults to the room of the event being answered.

A reject is 403 with the model's errcode. Silence, a failed call, an invalid reply, or a
reject mixed with an accept all fail closed: 500 `M_UNKNOWN` (or 503 `M_LIMIT_EXCEEDED` with
`Retry-After` when overloaded) with a category, never the error, and nothing is created or
delivered. Each decision is logged as `decision=model_answer|model_reject|model_silent|
fail_closed_*`; logins checked by Rust as `password_match|password_mismatch`.

## Bounds

Request body `MAX_BODY_BYTES` (1 MiB) → 413 `M_TOO_LARGE`; header and body deadlines 30 s;
64 headers, 32 KiB of header bytes; `DEFAULT_MAX_CONNECTIONS` with a JSON 503 past it.

## Not implemented

Federation, end-to-end encryption (`keys/*` is 404), media, registration, presence, typing,
receipts, redactions, aliases, room state endpoints, power levels (every member may invite).
