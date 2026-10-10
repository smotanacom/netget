# Matrix server tests

No LLM calls: a python policy is the model. `NETGET_MATRIX_PYTHON` names a python with
matrix-nio 0.26.0; the tests fail rather than skip without it.

## `matrix_nio_users_talk_through_netget`

`nio_session.py` drives two matrix-nio clients, which validate every answer against nio's own
JSON schemas. The steps:

1. A wrong password: `LoginError`, `M_FORBIDDEN`.
2. alice and bob log in.
3. alice creates a room inviting bob. The policy greets the room through `matrix_send`.
4. bob sees the invite (with the room's name) in `/sync` and joins.
5. alice sends "hello", then sends one transaction twice: the same event id comes back.
6. bob reads everything through `/sync`, in order: the greeting, hello and its echo, once and
   its echo.
7. bob's "forbidden" is refused (403 with the policy's errcode and text).
8. Then `joined_members`, `joined_rooms`, `/messages` (the refused message absent), `whoami`,
   and `logout` (the token is then unknown).

The test also asserts what the model was shown: one join event, three message events (the
duplicate transaction was not asked twice), and no login event, because `user_passwords`
decides logins.

## `refusals_bounds_and_long_poll`

Raw HTTP:

- missing and unknown tokens;
- an unknown room;
- a non-JSON body;
- one byte past `MAX_BODY_BYTES`;
- an unknown endpoint;
- a sync that waits for its timeout and returns the same batch;
- a waiting sync woken early by a new room.

## `a_failed_handler_refuses_and_delivers_nothing`

An unreachable model. `createRoom` fails closed with a category, not the error, and no room
exists.
