# LPD client tests

- `session_test.rs` runs the client against a scripted fixture that asserts the reachability
  probe, the exact control file (H, P, J, L, N, the format line and U) and data file of a job
  sent control-first, a refused final acknowledgement reported as `refused_at: final`, a long
  listing requested by the result handler, an injected data-first job in literal format, an
  injected removal and print-waiting command, and an injected action refused before the wire.
- `real_server_test.rs` prints to LPRng 3.8.B's `lpd`, started unprivileged on a free port,
  and asserts the document byte for byte from LPRng's spool directory, LPRng's own listing,
  and LPRng's refusal of an unknown queue. It fails, never skips, without LPRng:
  `sh scripts/test-peers/install-lprng.sh <root>` installs it and writes the `/etc/lprng`
  configuration LPRng reads (its `LPD_CONF` override is compiled out).

```bash
cargo test --no-default-features --features tcp,lpd --test client -- lpd:: --test-threads=4
```
