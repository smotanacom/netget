# Matrix client tests

No LLM calls: a python chain is the model. The homeserver is **Synapse**, the reference
implementation, started through `RealServer` from `NETGET_MATRIX_PYTHON`:

- `synapse_conf.py` has Synapse generate its own config, then points it at a probed loopback
  port and the temp dir, logs to the console, lifts rate limits and disables federation.
- `register_new_matrix_user`, from the same venv, creates alice and bob.

The test fails rather than skips without that python.

The other participant is bob, a **matrix-nio** client (`nio_bob.py`). Everything asserted
about what NetGet said is what bob read from Synapse.

## The chain

1. On login, NetGet creates a room inviting bob.
2. On the room's creation, it says hello.
3. On bob's join, it welcomes him.
4. On his "ping", it answers "pong".

## What is asserted

- bob was invited by alice.
- bob read, in order: hello bob, the welcome, pong, and a message the operator injected.
- NetGet saw bob's join and his ping.
- An injected `matrix_messages` returns Synapse's own history, newest first.
- Synapse refuses a send into a room NetGet is not in, and the refusal reaches the handler as
  `matrix_response` with an errcode.
- An invalid room id is `Rejected` locally.
- A wrong password fails the connect with `M_FORBIDDEN`, without the password in the error.

Mutation-checked: discarding the model's actions stalls the chain before the invite.
