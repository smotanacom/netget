# A2S server tests

- `wire_test.rs` sends raw datagrams: info answered without a challenge (and decoded
  field by field, extra data included), players asking for a challenge, a wrong challenge
  answered with the right one, players and a 150-rule answer split across datagrams none of
  which exceeds 1400 bytes, `info_challenge` making info ask too, and no reply for a
  wrong-kind answer, `a2s_refuse`, a missing handler, a request over 1400 bytes or junk.
- `real_client_test.rs` runs python-a2s 1.4.2 (with and without the info challenge) and
  woozymasta/a2s v0.4.0's Go client, each reading info, players and the split rules, each
  failing when absent.

```bash
cargo test --no-default-features --features tcp,a2s --test server -- a2s:: --test-threads=4
```
