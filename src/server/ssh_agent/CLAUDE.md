# SSH Agent Server Implementation

## Overview

An SSH agent on a Unix domain socket. Real agents hold private keys and sign challenges with
them; this one holds nothing — a handler (script, static or LLM) answers every request, so the
identities it lists and the signatures it returns are invented.

**Status**: Experimental
**Transport**: Unix domain socket (`#![cfg(unix)]`; no Windows named-pipe support)
**Privilege**: none — a socket path, not a privileged port
**Spec**: draft-ietf-sshm-ssh-agent-05
**Feature**: `ssh-agent`
**Files**: `mod.rs` (framing, wire parsing, responses), `actions.rs` (events, actions, metadata)

## The signature caveat

No private key exists here, so `send_sign_response` returns bytes the handler made up. A real
SSH server will reject them — agent authentication cannot succeed against a genuine peer. What
this *is* good for: observing what a client asks for, honeypots, and exercising client-side
agent code paths.

Similarly, `ssh_agent_lock` / `ssh_agent_unlock` and key constraints (`ssh-add -t`, `-c`) are
reported to the handler but **not enforced** by anything. If the agent should behave as locked,
the handler has to remember that itself and refuse later requests.

## Architecture

### Message framing

The wire format is `uint32 length || byte type || payload`. The reader accumulates bytes and
drains every complete frame:

```
read() -> append to pending -> while take_framed_message(&mut pending) -> handle
```

This matters. The previous reader treated each `read()` as exactly one message, so a client
pipelining two requests into one segment had the second silently discarded, and a request split
across two segments failed to parse. Frames are capped at 256 KiB (`MAX_AGENT_MESSAGE_LEN`) —
the length prefix is attacker-controlled, and without a cap a client could announce a huge
message and make the server buffer unboundedly. A zero-length or oversized frame closes the
connection.

Messages are handled **sequentially and inline**. Spawning a task per read let concurrent
requests race for the connection lock and be answered out of order, which a protocol that is a
strict request/response sequence on a single socket cannot tolerate. The
Idle/Processing/Accumulating state machine in `ConnectionData` therefore rarely leaves Idle; it
is retained but is no longer what provides ordering.

### Lifecycle

1. A stale socket at `socket_path` is unlinked — but only if it really is a socket. The path
   comes from a startup parameter, i.e. ultimately from model output, and unconditionally
   removing whatever is there would let a bad value delete a regular file. Anything else at
   that path is a hard startup error.
2. `UnixListener::bind` — failure propagates with `?`, so `server_startup` reports `Error`
   rather than a phantom `Running`.
3. The accept-loop `JoinHandle` is registered with `AppState::register_server_task()`, so
   `stop_server` aborts it and releases the socket.
4. Per connection: split the stream, register it in `ServerInstance` **and in the server's own
   connection map**, then spawn the `ssh_agent_connection_opened` task and the reader task.

The connection-map insert is step 4 and happens synchronously in the accept loop, before either
task exists. It used to be the first thing the connection-opened task did, racing the reader
task spawned immediately after it, and `handle_data_with_actions` returns silently when the
connection is not in the map. For an agent that is worse than a dropped byte: the frame has
already been taken off the read buffer, the protocol is a strict request/response sequence with
no retry, so the client blocks forever on a reply that was never generated — `ssh-add -l` hangs.
7 of 64 clients that wrote immediately after connect hung this way against the pre-fix binary.
`tests/connection_map_race_test.rs` covers it; the same shape was fixed in `socket_file`
(`1f3945ee`), `tcp` and `tls`.

### Responses

`send_response` prepends the length and writes. It waits on the write lock; it used to
`try_lock()` and, on failure, silently drop the response — leaving the client blocked forever
on a reply that was never sent.

Hex fields from the handler are validated. `public_key_blob_hex` and `signature_hex` that are
not valid hex, or that decode to zero bytes, now fail the request with `SSH_AGENT_FAILURE`
instead of putting an empty blob on the wire that the client reads as a valid-but-empty key or
signature.

## LLM Integration

Every event goes through `call_llm` → `try_execute_event_handler`, so script and static
handlers run in-process at **zero** LLM calls.

### Events

| Event                             | Msg | Parameters |
|-----------------------------------|-----|------------|
| `ssh_agent_connection_opened`     | –   | `connection_id` |
| `ssh_agent_request_identities`    | 11  | – |
| `ssh_agent_sign_request`          | 13  | `key_type`, `public_key_blob_hex`, `data_hex`, `flags` |
| `ssh_agent_add_identity`          | 17, 25 | `key_type`, `public_key_blob_hex`, `comment`, `constrained` |
| `ssh_agent_remove_identity`       | 18  | `public_key_blob_hex` |
| `ssh_agent_remove_all_identities` | 19  | – |
| `ssh_agent_lock`                  | 22  | `passphrase` |
| `ssh_agent_unlock`                | 23  | `passphrase` |

Two things were fixed here. Every event used to declare a required `connection_id` parameter,
but `parse_message` emitted `{}` for most of them, so the field was documented and never
delivered; only `ssh_agent_connection_opened` genuinely carries one, and the rest no longer
claim to. And `lock`/`unlock` parsed the passphrase off the wire and threw it away, delivering
an empty event — a handler could not tell a correct unlock passphrase from a wrong one, which
is the entire point of the operation.

`sign_request` now also carries `key_type` (`"ssh-ed25519"`, `"ssh-rsa"`, …) decoded from the
key blob, so a handler can identify the key without decoding hex itself.

### Actions

| Action                  | Sends                        | Answers |
|-------------------------|------------------------------|---------|
| `send_identities_list`  | `SSH_AGENT_IDENTITIES_ANSWER` | `request_identities` |
| `send_sign_response`    | `SSH_AGENT_SIGN_RESPONSE`    | `sign_request` |
| `send_success`          | `SSH_AGENT_SUCCESS`          | add / remove / remove_all / lock / unlock |
| `send_failure`          | `SSH_AGENT_FAILURE`          | any (refusal) |
| `close_connection`      | nothing — drops the socket   | any |
| `wait_for_more`         | nothing                      | rarely correct |

Returning **no** action is refused, not ignored. An agent request is a question the client
blocks on, so an answer with nothing usable in it, an action whose fields will not decode, and
an outright LLM error all send `SSH_AGENT_FAILURE` and log a `decision=fail_closed_*` token:

| Situation | Wire result | Logged decision |
|---|---|---|
| Handler returns `send_success` / a real reply | that reply | (none; the reply is the record) |
| Handler returns `send_failure` | `SSH_AGENT_FAILURE` | (none; the model's own refusal) |
| Handler returns no usable action | **`SSH_AGENT_FAILURE`** | `decision=fail_closed_no_action` |
| An action's fields will not decode | **`SSH_AGENT_FAILURE`** | `decision=fail_closed_action_error` |
| LLM call errors or times out | **`SSH_AGENT_FAILURE`** | `decision=fail_closed_llm_error` + `category=` |

`wait_for_more` is the one non-answer that stays silent, deliberately: it means "I want the
rest of a pipelined request before deciding". Nothing on any path produces
`SSH_AGENT_SUCCESS` or an identity list unless a handler asked for it by name, so an LLM
outage cannot become an affirmative answer. `tests/server/ssh_agent/e2e_test.rs`'s
`unanswered_request_fails_closed` pins this; it was verified to fail when the rule is disabled.

Refusing explicitly with `send_failure` is still better practice — it distinguishes a decision
from an outage in the log.

The `ssh_agent_connection_opened` event is exempt: no request is pending there, so an
unsolicited `SSH_AGENT_FAILURE` would answer a question the client never asked.

There are no async (user-triggered) actions. `modify_instruction` produced an
`ActionResult::Custom` that the executor ignored — the common `update_instruction` action does
the job. `close_connection` declared a `connection_id` parameter the executor discarded (it
always closed the connection that raised the event), so it moved to the sync actions without
that parameter.

### Hex in event data

The action-design rule bans raw bytes and base64. Hex is used here deliberately: SSH key blobs
and signatures *are* opaque byte strings, there is no structured alternative, and hex is the
one encoding a model can read and write digit by digit. Where structure is available it is
provided alongside — hence `key_type` and `comment` next to the blob.

## Storage

None. No keys are persisted, no file is written apart from the socket itself. Keys a client
adds exist only in whatever the handler chooses to remember.

## Known limitations

1. **Fabricated signatures** — see above.
2. **ADD_IDENTITY parsing assumes the Ed25519 layout** (`key_type`, public blob, private blob,
   comment). RSA and other multi-field key types have a different field sequence, so
   `public_key_blob_hex` and `comment` may be wrong or the message may fail to parse. The event
   parameter descriptions say so.
3. **Lock/unlock and constraints are not enforced** — reported only.
4. **No OpenSSH extensions** (`session-bind@openssh.com`, `restrict-destination-v00@…`).
5. **No smartcard operations** (`ADD_SMARTCARD_KEY`, `REMOVE_SMARTCARD_KEY`).
6. **Unix only** — Windows would need a named-pipe implementation.
7. The socket is created with the process umask; a real agent socket should be 0600.

## Testing

`tests/server/ssh_agent/e2e_test.rs` exists (unlike ssh, telnet and ftp). Manual check:

```
SSH_AUTH_SOCK=./netget-ssh-agent.sock ssh-add -l
```

## Example prompts

### Two fixed keys

```
Start SSH Agent on ./netget-ssh-agent.sock
On ssh_agent_request_identities return two identities: admin-key and deploy-key
Refuse every sign request with send_failure
```

### Learning agent

```
Start SSH Agent on ./netget-ssh-agent.sock
Remember every key added with ssh_agent_add_identity (key_type and comment) and
list them back on ssh_agent_request_identities
Accept remove and remove_all with send_success
```

## References

- IETF draft-ietf-sshm-ssh-agent-05
- OpenSSH `authfd.h`: https://github.com/openssh/openssh-portable/blob/master/authfd.h
- RFC 4251 §5 (SSH binary data types)
