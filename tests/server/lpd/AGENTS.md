# LPD server tests

- `wire_test.rs` drives the server over raw sockets with a Python script and static rules:
  jobs with the control file first and data first, the handler's decision carried by the
  final acknowledgement (0 queued, 1 refused), several jobs on one connection, binary data
  shown to the handler as `text: null`, the abort subcommand, a queue outside `queues` refused
  at once, short and long listings and removal rendered from the handler's answer,
  print-waiting closing without a reply, the bounds (a control file past 64 KiB and data past
  `max_job_bytes` refused at their announcement, a file without its NUL terminator and an
  over-long command line closed), and the fail-closed paths with no handler and an
  unreachable model.
- `real_client_test.rs` runs two independent clients, each failing when absent: LPRng 3.8.B's
  `lpr` (an accepted and a refused job), `lpq` and `lprm`, and CUPS's `lpd` backend in both
  file orders. The handler records every job event, so what each client sent is asserted from
  the server's side.

`sh scripts/test-peers/install-lprng.sh <root>` installs both (with sudo) and prints the
`NETGET_LPRNG_ROOT`, `NETGET_LPRNG_SPOOL` and `NETGET_CUPS_LPD_BACKEND` exports.

```bash
cargo test --no-default-features --features tcp,lpd --test server -- lpd:: --test-threads=4
```
