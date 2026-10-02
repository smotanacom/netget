# Gearman client validation

Run both suites through the programme build wrapper:

```sh
python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features tcp,gearman --test client --test server -- gearman:: --test-threads=100
```

`real_server_test.rs` owns an independent gearmand daemon through RealServer, with
loopback ephemeral ports and a private PID path. Its process group is reaped on success,
failure and panic. `gearmand` and `gearman` are required and missing binaries fail;
install `brew install gearman` or `apt install gearman-job-server gearman-tools`.
Python3 drives the CLI worker's transform; no global Python packages are needed.
Local independent peer: Homebrew gearmand/gearman 2.1.0, installed in
/opt/homebrew/sbin and /opt/homebrew/bin. Source checked:
https://github.com/gearman/gearmand/tree/2.1.0/libgearman-server.
The inspected source archive SHA256 is
`4d24340ab39be851b40d895687c17d6d16e730ece1fa9d9294d6b2b0a8cb1261`.

Independent evidence covers a NetGet submitter with the CLI worker through gearmand:
three priorities, unique keys, data/completion, background, echo and completed status.
The CLI producer with a NetGet worker through gearmand verifies ability registration,
GRAB_JOB/GRAB_JOB_UNIQ, NO_JOB/PRE_SLEEP/NOOP, unique assignment keys, progress,
intermediate data/warning, complete, fail and exception (libgearman's unnegotiated
exception becomes WORK_FAIL). A negotiated exception is also checked as a typed
terminal event between two NetGet clients through the independent daemon. The mocked
model submits a real job to the CLI worker and shared memory reaches both followups
(three model calls). Published static/script examples are executed against the CLI
worker with function_name; a script worker registers and answers the CLI producer.

`session_test.rs` verifies the existing NetGet submitter/server pair (progress, data,
completion, status and background) and its deliberate worker refusal; fragmented
JOB_CREATED survives busy injection and a binary completion event survives EOF; three
tracked tasks and responsive disconnect/removal while a manual handler/request is
parked; role/assigned-handle/ability bounds and 64 job/assignment bounds; wrong response
types/echo/status/flags/handles, unknown handles and final ERROR; event queue/followup
bounds; whole request reply deadline and resulting peer EOF. Access-log polling selects
the earliest matching event after the cursor so fast multi-packet responses cannot
silently omit progress/data evidence.

`wire_test.rs` checks six submit variants, all selected worker request types and
validation; exact largest request/response bodies and one-past limits; wrong response
magic and overlarge size before allocation; partial-header/body and blocked-write
whole deadlines; busy rejection and disconnect during a blocked write. Tests live
outside src and contain no ignore or skip gates. Maturity remains Experimental.

Ubuntu 24.04's primary package pages currently list both gearman-job-server and
gearman-tools as 1.1.20+ds-1.2build4:
https://packages.ubuntu.com/noble/gearman-job-server and
https://packages.ubuntu.com/noble/misc/gearman-tools.
The fixtures require compatible wire/CLI behavior, without asserting a specific
version; local evidence above is 2.1.0. The Linux packaged-peer run is separate evidence.
