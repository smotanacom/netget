# rsync client tests

No LLM calls. `real_server_test.rs` runs a **stock rsync 3.2.7 daemon** (`rsync --daemon`,
from a temp-dir config with `use chroot = no`). It adds `uid = 0`/`gid = 0` only when run as
root, because a root daemon would otherwise serve modules as `nobody`. The daemon runs at
protocol 31 and steps down to NetGet's 29.

- The chain:
  1. List the modules; this checks the MOTD and both modules.
  2. List `pub/` recursively; the result is in rsync's order.
  3. Fetch the one `.md` file in the listing; its bytes exist only on the daemon.
- Injected actions:
  - a binary file comes back as hex;
  - an unknown module is the daemon's refusal;
  - the protected module without credentials says authentication is required.
- `password_module`: alice's password fetches from `secret` through the MD4
  challenge-response. A wrong password gets the daemon's `auth failed`.

rsyncd's own log cannot be used as evidence: run as root, it stops after "allowed access"
even for stock client against stock daemon.

Mutation-checked: discarding the model's actions fails the chain at `rsync_modules`.
