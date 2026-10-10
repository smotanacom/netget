# ActivityPub server tests

`real_client_test.rs` runs against **Fedify 2.4.2** (npm), an independent TypeScript
implementation used by Hollo, Ghost's federation and others. `install_peers.py <dir>` installs
`@fedify/cli`, `@fedify/fedify` and `@fedify/vocab` and prints `NETGET_FEDIFY` (the CLI) and
`NETGET_FEDIFY_PEER` (`peer/peer.mjs`). The tests **fail** without them.

- `fedify_reads_and_follows_netget`: `fedify webfinger` resolves the actor and `fedify lookup
  -C` parses it as JSON-LD (it rejects a malformed document). Then `peer.mjs follow` — an actor
  built on the Fedify library — follows NetGet's actor with a signed Follow. A python policy is
  the model: it accepts and posts a note. Fedify's inbox prints only what it verified, so the
  test asserts the Accept and the Create arrived signed by NetGet's actor, with the note's
  content and audience, and checks the model's event, the followers collection, the outbox and
  the note document.
- `unsigned_and_forged_requests_are_refused`: raw HTTP — no signature, a signature by the wrong
  key, a digest that does not match the body (all 401), a body over `MAX_DOCUMENT` (413), and
  an unknown WebFinger account (404). None reaches the model.
- `a_failed_handler_accepts_nothing`: with the model unreachable, a Follow is not accepted and
  the actor has no followers.

Why the library and not `fedify inbox`: the CLI's ephemeral inbox binds `::`, refuses private
addresses, skips signature verification unless told otherwise, and crashes printing a
localhost id. `peer.mjs` uses the same library with `allowPrivateAddress`, as Fedify's own
tests do, and verifies every signature.

No LLM calls.
