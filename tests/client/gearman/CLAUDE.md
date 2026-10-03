# Gearman client validation

Run both suites through the programme build wrapper:

```sh
python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features tcp,gearman --test client --test server -- gearman:: --test-threads=100
```

`real_server_test.rs` owns an independent gearmand daemon through RealServer, with
loopback ephemeral ports and a private PID path. Its process group is reaped on success,
failure and panic. `gearmand` and `gearman` are required and missing binaries fail;
install `brew install gearman` or build the pinned Linux peer below.
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
version. The selected worker exception gate requires the official 2.1.0 peer:

```sh
scripts/test-peers/install-gearman-linux.sh /absolute/owned/gear-peer
PATH=/absolute/owned/gear-peer/bin:$PATH python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features tcp,gearman --test client --test server -- gearman:: --test-threads=100
```

The installer verifies the official release's SHA256 above, keeps TLS verification,
disables optional persistent backends, and builds only gearmand/gearman/gearadmin
with two jobs and configure/build deadlines inside the caller's peer root. Install
its documented build dependencies separately. Linux runtime validation belongs to
the blocking CI job; syntax/checksum inspection alone is not a Linux passing run.

An actual local upstream 1.1.20 build (release SHA256
`2f60fa207dcd730595ef96a9dc3ca899566707c8176106b3c63ecf47edc147a6`)
passed 18/19 client tests and failed the CLI producer's unnegotiated exception.
Complete, ordinary failure and negotiated exception work. Two independent raw
clients reproduced the daemon defect without NetGet: JOB_CREATED body `H:test:1`
has 8 bytes; WORK_EXCEPTION body `H:test:1` + NUL + `exception text` is converted
into WORK_FAIL with a **9-byte body `H:test:1` + NUL**. The single-field WORK_FAIL
must contain the original handle, so libgearman cannot correlate it and waits.
In 1.1.20 `_server_queue_work_data` copies the exception argument size including
its delimiter into the failure packet. Ubuntu's 1.1.20+ds-1.2build4 packaging
patches (typos, documentation, Boost multiarch, version/VCS) do not change packet
or server exception handling. This is upstream peer incompatibility; no scenario
is skipped and NetGet's handle validation remains strict.

Primary sources: https://github.com/gearman/gearmand/tree/1.1.20/libgearman-server,
https://archive.ubuntu.com/ubuntu/pool/universe/g/gearmand/gearmand_1.1.20+ds-1.2build4.debian.tar.xz.
The temporary local compatibility peers were resolved through PATH at
`/private/tmp/netget-gearmand-1.1.20-20261002/bin`; both daemon and CLI used 1.1.20.
