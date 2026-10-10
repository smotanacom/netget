# BFD client tests

No LLM calls. **BIRD 2** (`apt-get install bird2`), unprivileged, with a static multihop
session to 127.0.0.4, where NetGet's client listens (`local_address`). Read back with
`birdc show bfd sessions`. Fails rather than skips without it. Both tests hold a static mutex
on port 4784.

- Up; then the model's `bfd_set_timers` (rx 400 ms) makes BIRD's interval 0.400.
- Injected AdminDown (BIRD goes Down) and back Up. An invalid diag is Rejected.
- `disconnect` sends AdminDown, so BIRD goes Down at once.
- Meticulous keyed MD5 with key id 7 comes Up.

Mutation-checked: discarding the model's actions fails the session test at "Up at 0.400".
