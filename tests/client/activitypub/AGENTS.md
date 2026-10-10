# ActivityPub client tests

`real_server_test.rs` runs `tests/server/activitypub/peer/peer.mjs serve` — an actor built on
the **Fedify 2.4.2** library that accepts every Follow — and needs `NETGET_FEDIFY_PEER` from
`tests/server/activitypub/install_peers.py`; it fails without it.

`netget_follows_and_posts_to_a_fedify_actor`: a python chain is the model. On
`activitypub_ready` it follows the peer; Fedify verifies the signed Follow (it prints only what
verified), answers with a signed Accept that NetGet must verify to raise
`activitypub_activity{type: Accept}`, and the chain answers that with a note Fedify receives,
fetches and parses — content and `to` asserted, and the note id matched against what NetGet
reported. Then an injected `activitypub_lookup` of `peer@127.0.0.1:<port>` resolves Fedify's
WebFinger and actor document, and an injected `activitypub_accept` is refused locally.

Mutation-checked: dropping the model's actions in the dispatcher fails the test (no Follow).
No LLM calls.
