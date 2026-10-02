# Remaining audit implementation — 2026-10-02

This report records the follow-up implementation of the remaining audit backlog after the
2026-10-01 review. It covers the twelve remaining recursive-decoder exceptions, the two
stale wrapper exceptions, concrete wrapper and peer-task ownership fixes, and the CI changes
that exercise them. It is an implementation and compatibility record, not a declaration
that every protocol has independent interoperability coverage.

The work was integrated in `/Users/matus/dev/netget/.audit-integration-20261002`. The existing
review snapshot was preserved before these changes. Parallel protocol-expansion worktrees
were outside this task. No GPU, real model, external model endpoint, physical USB device,
or other hardware was used for the checks described here.

## Verification status at report preparation

The coordinating agent confirmed these completed results:

| Check | Confirmed result | Scope |
|---|---|---|
| `audit_server_decoder_bounds_test` | 10 passed | Four IPP, three DHT, three FIDO2 tests; in-memory fixtures |
| `grpc_value_bounds_test` | 7 passed | Both public peer conversion paths; in-memory schema/value fixtures |
| `recursive_decoder_depth_test` | 3 passed | Empty unbounded set, known bounded identities, scanner fixtures |
| `spawned_netget_is_tied_test` | 3 passed | Death-tie and process-spawn recognition plus repository ratchet |
| `task_registration_cancellation_test` | 6 passed | Client/server/peer cancellation and successful ownership transfer |
| `wrapper_lifecycle_test` | 5 passed | E2E wrapper fixtures and included wrapper unit tests |
| Changed server/ratchet Rust files | Formatting and focused diff checks passed | Formatting/static validation only |

The coordinating agent confirmed **34 passing executions** across the six targets above.
The four PTY fixtures and broader 324-test native regression run also passed after this
checkpoint. Final fixture corrections, re-lint, Windows, and other integration outcomes
remain in the separate final verification record. This report deliberately does not
invent a final aggregate, convert unexecuted platform fixtures into passes, or reuse older
review totals as evidence for this implementation.

**Final machine-readable verification is separate.** The final verification artifacts are
the authority for exact commands, feature sets, exit statuses, logs, platform availability,
and final totals. A CI definition shows that a check is scheduled; it does not prove that
the remote runner has executed it.

## Changed-path inventory

Paths below are relative to the isolated integration worktree. Shared targets appear once.

| Area | Exact changed paths |
|---|---|
| IPP | `src/server/ipp/actions.rs`; `src/server/ipp/CLAUDE.md` |
| DHT | `src/server/torrent_dht/mod.rs`; `src/server/torrent_dht/CLAUDE.md` |
| FIDO2 | `src/server/usb/fido2/mod.rs`; `src/server/usb/fido2/CLAUDE.md` |
| IPP/DHT/FIDO2 regression target | `tests/audit_server_decoder_bounds_test.rs` |
| Shared gRPC conversion | `src/server/grpc/value_codec.rs`; `src/server/grpc/mod.rs`; `src/client/grpc/mod.rs` |
| gRPC documentation and tests | `src/server/grpc/CLAUDE.md`; `src/client/grpc/CLAUDE.md`; `tests/grpc_value_bounds_test.rs` |
| Recursion ratchet | `tests/recursive_decoder_depth_test.rs` |
| E2E wrapper ownership | `tests/e2e/netget_wrapper.rs`; `tests/helpers/wrapper_lifecycle.rs`; `tests/wrapper_lifecycle_test.rs` |
| PTY wrapper ownership | `tests/terminal_snapshot.rs`; `tests/terminal_snapshot/mod.rs`; `tests/terminal_snapshot/child_lifecycle.rs` |
| Wrapper ratchet | `tests/spawned_netget_is_tied_test.rs` |
| Peer-task registration | `src/state/app_state.rs`; `tests/task_registration_cancellation_test.rs` |
| Windows fixture portability | `Cargo.toml`; `tests/process_group_windows_test.rs` |
| CI | `.github/workflows/ci.yml` |
| Build invalidation inputs | `build.rs` |
| This implementation record | `audit/2026-10-02/remaining-audit-implementation.md` |

The server review agent implemented IPP/DHT/FIDO2, their shared focused regression target,
their local documentation, and the recursion-ratchet update. The client review agent
implemented the shared gRPC codec and its integration/tests. The surfaces review agent
implemented wrapper and platform fixtures. The coordinating agent integrated the changes,
implemented the peer-task registration and build-input corrections, and owns final
verification and publication.

## 1. IPP attribute validation and JSON fallback fields

### Before

`validate_attributes` contained a recursive `check` over arrays. It validated integer ranges
and raw string lengths but relied on the caller's JSON parser to restrict nesting. A directly
constructed value did not have that incidental parser protection. Objects were skipped by
this check even though the response encoder serialized objects and nested arrays as JSON text.

That fallback introduced a second concrete problem: a value could pass validation and then
serialize to more than the IPP two-byte value-length field can express. The low-level encoder
clamped the bytes to `u16::MAX`, producing a self-consistent but truncated representation.
Escaping could make the serialized text exceed the limit even when individual raw strings
were shorter than the limit.

### Implemented behavior

- `MAX_IPP_ATTRIBUTE_DEPTH` is 32 array/object levels. The surrounding attribute map is not
  counted. A scalar consumes no container level.
- An explicit iterative walk covers both arrays and objects before any JSON fallback
  serialization. It rejects excess depth with an attribute-specific error.
- Existing integer-range checks are preserved for ordinary scalar attributes and arrays.
  Numbers inside an object continue to use JSON syntax; they are not incorrectly forced
  into IPP's signed 32-bit integer syntax.
- The validation follows the actual encoder shape: the outer array is an IPP set; each
  nested array or object is one JSON-text field.
- A bounded `std::io::Write` implementation counts serialized JSON bytes without allocating
  an oversized temporary string. It refuses a field that cannot be encoded in full.
- The borrowed `validate_attributes` entry point and public depth constant permit direct
  validation of programmatically constructed values.

### Regression evidence

The four IPP tests cover array-only and mixed object/array nesting at 32 levels and at 33;
20,000-level directly constructed trees; exact 65,535-byte JSON fallback fields; excess
serialized length including escaped characters; nested container fallback; ordinary sets;
and large/fractional numbers inside JSON objects.

The deep-tree fixture dismantles the input iteratively after validation. This isolates the
validator from the standard recursive destructor of `serde_json::Value`; the test does not
claim to change that dependency's destruction behavior.

### Compatibility

Ordinary representable attributes retain their encoding. Values deeper than the new local
limit, or fallback JSON that previously became truncated text, now return errors. This is an
intentional refusal of values that cannot be processed under the documented bounds. No
partial field or substituted null is returned. This work does not add recursive parsing of
incoming IPP attribute groups; the existing inbound header parser remains a separate path.

## 2. DHT conversion has a local bound

### Before

`parse_krpc_message` already ran the iterative shared bencode structure check before
deserialization. That preflight capped wire nesting at 32, but `bencode_to_json` itself was
an infallible recursive walker. Its safety depended on callers always reaching it through
the preflight, and the protocol-local static scan could not see the bound in `src/utils`.

### Implemented behavior

- `MAX_DHT_VALUE_DEPTH` explicitly references the existing shared 32-level bencode limit.
- `bencode_to_json` returns `Result<serde_json::Value>` and calls a bounded recursive helper.
- Each list or dictionary consumes one level; scalar values do not consume a level.
- The check occurs before descending into an excess-depth container.
- Recursive errors propagate to `parse_krpc_message` with `?`; the converter never returns
  a shortened tree or a fabricated value.
- The original wire preflight remains in place before `serde_bencode` deserialization.

### Regression evidence and compatibility

Three tests cover integers, printable text, binary/non-ASCII hex conversion, ordinary
dictionaries and lists, exact depth 32, depth 33, and 20,000-level in-memory trees. The deepest
accepted tree is traversed to its original scalar leaf to establish that success did not
mean truncation. Deep test values are dismantled iteratively.

Existing accepted KRPC wire messages retain their conversions: the original 32-level wire
preflight is still at least as restrictive as conversion of a nested argument. Printable
bytes still become text, binary/non-ASCII bytes still become hex, and dictionary key
conversion is preserved. Direct conversion now exposes an explicit failure result and is
safe independently of the wire-parser calling convention.

## 3. FIDO2 dispatch and parking are acyclic

### Before

`run_command` called `park` when CTAP2 or U2F required user presence. If no approval channel
was attached, or its receiver was closed, `park` called `run_command` again with
`UserPresence::Denied`.

The cycle terminated because the current handlers never asked for approval after denial.
That was a semantic invariant rather than an explicit structural bound: a future handler
change could accidentally make the cycle recurse again.

### Implemented behavior

`park` now calls `deny_presence_command`, a leaf helper that directly invokes the appropriate
CTAP2 or U2F handler once with `Denied`. It never enters dispatch or parking. The normal
encoded denial/error result is framed for the original channel and command.

If a handler incorrectly returns `NeedsApproval` after denial, the helper emits an error log
and returns the protocol's denial immediately: U2F `0x6985` or CTAP2 `0x27`. It does not
retry, recurse, or create a credential. Unsupported command kinds receive CTAPHID
`InvalidCmd`.

### Regression evidence

Three tests drive the actual public `UsbInterfaceHandler` with in-memory interface and HID
frames. They do not bind USB/IP, enumerate devices, or obtain a physical device handle.

- Absent and closed approval channels are tested for both U2F and CTAP2 over repeated
  registrations. Each response has the expected denial bytes, leaves no pending response,
  and stores no credentials.
- An attached approval channel preserves `KEEPALIVE(UPNEEDED)`, denied registration,
  approved registration, and channel-busy behavior when another command arrives while
  approval is pending. Approved registration exercises CPU cryptography.
- PING accepts the exact 7,609-byte CTAPHID maximum. A 7,610-byte declaration receives the
  expected length error. Malformed U2F/CTAP2 payloads retain their protocol errors, and a
  subsequent valid PING still succeeds.

### Compatibility

Normal approval, denial, and framing semantics are preserved. The new defensive branch
only matters if a handler violates the denied-presence invariant. These tests establish
in-memory handler behavior, not interoperability with libfido2, browsers, physical security
keys, or OS USB/IP clients.

## 4. Shared gRPC value conversion

### Before

The client and server each maintained their own recursive JSON/protobuf conversion helpers.
Their nesting protection depended on serde/prost parser defaults. Directly constructed
values did not receive the same protection. Some invalid shapes were silently transformed:
unknown fields were logged and discarded, non-object messages could become empty messages,
and conflicting fields or keys could overwrite earlier values.

### Implemented behavior

Both peers now expose conversion entry points backed by
`src/server/grpc/value_codec.rs`. One shared budget follows every recursive path and sibling:

| Bound | Value | Meaning |
|---|---:|---|
| Depth | 32 | Root depth zero; each message field, map value, or list element adds one |
| Nodes | 100,000 | Aggregate logical nodes across the whole conversion |
| Retained content | 8 MiB | Accounting includes values, strings/base64, node overhead, and key overhead |
| Node accounting | 64 bytes | Per logical node, charged before retaining its representation |
| Map-key accounting | 32 bytes plus text | Charged in addition to the key's content |

These are conversion budgets, not a replacement for the existing 4 MiB inbound gRPC wire-body
cap. Checks occur before large copies, base64 output, or decoded-byte allocation. Exceeding
a bound produces the typed `ValueLimitExceeded` error; complete values or errors are returned.

Validation also rejects unknown fields, non-object messages, multiple members of one `oneof`,
map keys that normalize to the same protobuf key, float overflow, float underflow to zero,
non-finite reflected floats, invalid bytes/base64, and invalid scalar shapes. Field updates
use fallible reflection APIs. A request conversion resource error maps to gRPC status
`RESOURCE_EXHAUSTED` (8); invalid representational/schema input maps to `INVALID_ARGUMENT`
(3), preserving the distinction from internal failures.

### Regression evidence

Seven tests use in-memory protobuf descriptors without invoking `protoc`. They exercise both
public peer conversion paths, nested protobuf encode/decode round trips, message/list/map
depth boundaries, directly constructed deep reflected values, exact node and content limits,
aggregate sibling budgets, base64 expansion, malformed values, duplicate normalized keys,
`oneof` conflicts, non-finite values, and request error classification.

### Compatibility

Valid field-name JSON and protobuf messages inside the limits retain their round-trip
meaning. Callers that relied on unknown fields being ignored, non-object values producing
empty messages, conflicting keys/oneofs choosing a winner, or unrepresentable floating-point
values silently changing now receive errors. Large/deep conversions beyond the explicit
budgets are intentionally refused. This is stricter validation, not protocol expansion.

## 5. All twelve recursive-decoder exceptions are closed

The previous remaining exception identities map to these implemented changes:

| Former exception | Resolution |
|---|---|
| `client:grpc:mod.rs:json_to_dynamic_message` | Shared explicitly bounded codec |
| `client:grpc:mod.rs:json_to_field_value` | Shared explicitly bounded codec |
| `client:grpc:mod.rs:json_to_proto_value` | Shared explicitly bounded codec |
| `client:grpc:mod.rs:proto_value_to_json` | Shared explicitly bounded codec |
| `server:grpc:mod.rs:json_to_dynamic_message` | Shared explicitly bounded codec |
| `server:grpc:mod.rs:json_to_field_value` | Shared explicitly bounded codec |
| `server:grpc:mod.rs:json_to_proto_value` | Shared explicitly bounded codec |
| `server:grpc:mod.rs:proto_value_to_json` | Shared explicitly bounded codec |
| `server:ipp:actions.rs:check` | Iterative attribute walk and explicit container limit |
| `server:torrent_dht:mod.rs:bencode_to_json` | Fallible helper with its own depth parameter and bound |
| `server:usb/fido2:mod.rs:park` | Calls direct leaf denial, not dispatch |
| `server:usb/fido2:mod.rs:run_command` | Dispatch/parking cycle removed |

`tests/recursive_decoder_depth_test.rs` no longer contains an unbounded baseline. It requires
the discovered unbounded set to be empty. Its previous minimum-count assertion was removed:
sharing duplicate codecs and replacing recursion with iteration legitimately reduce counts.

Sensitivity is maintained through specific known bounded AMQP, VNC, XML-RPC, DHT and gRPC
identities, checks that removed IPP/FIDO2 cycle identities stay absent, historical unbounded
fixtures, bound-removal fixtures, and new FIDO2-cycle/iterative-IPP fixtures. The gRPC
`value_to_json` helper is a recognized `Value`-shaped recursive entry. The JSON-side `Json`
alias is outside the scanner's current type-shape vocabulary; its explicit budgets are
exercised by the runtime codec target. The source scanner remains a heuristic for call
cycles and visible bounds, not a proof of every parser's safety.

## 6. Wrapper ownership and the two stale exceptions

### Correction to the earlier exception descriptions

The old wrapper baseline listed `tests/e2e/netget_wrapper.rs` and
`tests/terminal_snapshot/mod.rs` as lacking an OS-level tie. Inspection showed that both
already called `arm_death_tie`; those descriptions were stale. This pass does not claim to
have introduced a previously absent death tie to those wrappers.

There were still concrete ownership defects to repair. Optional tie creation could fail
without refusing startup; cancellation could drop the sole child owner during shutdown;
and successful reaping was not consistently coordinated with disarming a numeric-PID tie.

### E2E wrapper changes

- Spawn enables `kill_on_drop(true)` and creates a private process group.
- `protect_child` requires successful death-tie arming. Failure terminates the owned process
  group, awaits reaping within a bound, and returns an error.
- `stop` retains the child inside the wrapper across every await. Cancelling that future
  does not detach the child's only owner while leaving the wrapper alive.
- Graceful shutdown has a one-second bound. Forced termination signals the owned group and
  waits up to two seconds for the child.
- `is_running` disarms the tie when `try_wait` has reaped the child, reducing the interval in
  which an armed numeric PID could refer to a reused PID.
- Drop handles an already-reaped child, otherwise terminates the owned group and reaps using
  the available runtime. The tie remains a backstop if reaping cannot be confirmed.

### PTY wrapper changes

The `NetGetChild` constructor requires a death tie before returning the wrapper. Failed
arming leaves an owned guard that kills and reaps during error cleanup. Drop recognizes
already-reaped children before signalling, retains bounded teardown, and leaves the tie
armed if termination/reaping cannot be confirmed. Unix PTY-specific test code is explicitly
gated to Unix platforms.

### Fixture and ratchet coverage

`tests/helpers/wrapper_lifecycle.rs` creates exactly owned inert shell/sleep fixtures and an
unrelated sentinel. Parent-death tests kill only their fixture owner; the sentinel must
survive. The inert child does not rely on stdin EOF for termination. The PTY child ignores
SIGHUP before announcing readiness, preventing terminal hangup from making a missing
death-tie implementation appear to pass.

A final fixture review removed a failure-only raw PID kill after a timeout: the orphan could
have exited and its numeric PID could have been reused. Inert children now check a unique
outer-owned temporary-file lease; outer cleanup revokes it on failure, and a bounded loop
also expires independently. The lease stays present throughout the parent-death assertion,
so it cannot hide a missing death tie. Both fixture paths reuse their wrapper's child-guard
module, resolving the duplicate-module strict lint without suppressing it.

The E2E fixtures cover abrupt parent death, graceful and forced stop, cancellation of stop,
and failed tie arming. PTY fixtures cover abrupt parent death, bounded normal teardown,
already-reaped children, and failed arming. Normal cleanup requires reaping; abrupt-parent
fixtures recognize an already-dead orphan zombie as terminated if the host init has not yet
reaped it. That distinction is explicit rather than a claim that the dead parent reaped it.

The wrapper ratchet now recognizes both `::tie_child` and `::arm_death_tie`. It does not
mistake `kill_on_drop` or `untie_child` for a tie. It distinguishes process `Command` spawning
from a protocol object's unrelated `spawn` method. The two stale exceptions were removed,
leaving an empty exception list. Source fixtures pin those recognition rules.

## 7. Peer-task registration retains ownership before first poll

`AppState::register_peer_task` previously accepted an already-running `JoinHandle` inside an
`async fn`. Dropping the returned future before its first poll could drop that handle without
aborting the running worker. A cancellation while awaiting the state lock had the same
ownership concern. This was the peer-specific counterpart of the already addressed
client/server task-registration pattern.

The method now constructs `PendingTaskRegistration` synchronously and returns an async block
that owns the guard. Dropping an unpolled or unfinished registration aborts the child. After
the server and peer are confirmed present under the state lock, ownership transfers into
the registered task collection. Missing owners return without disarming the guard.

Existing `.await` call sites retain their usage. Successful registration keeps the worker
alive until the peer is removed. Two added tests distinguish unpolled cancellation from
successful ownership transfer, and the successful-removal fixture also verifies that removing
one peer preserves its server. The target retains its existing client/server registration
coverage. The coordinating agent confirmed all six tests in the target passed; exact commands
and logs belong to the separate verification record.

## 8. CI integration and platform boundaries

The Linux `resource-lifecycles` job shares one explicit `AUDIT_CPU_FEATURES` set across
its native commands, including the original CPU features plus gRPC, IPP, DHT, FIDO2 and
terminal snapshots. The six new/extended targets run with the existing native regressions;
a separate command selects only `terminal_snapshot child_lifecycle`. Both use the same
feature set, avoiding separate full-library compilations. Build concurrency is two.

The new target selection is:

```sh
--test grpc_value_bounds_test --test audit_server_decoder_bounds_test \
--test wrapper_lifecycle_test --test task_registration_cancellation_test \
--test recursive_decoder_depth_test --test spawned_netget_is_tied_test
```

The PTY command is:

```sh
cargo test --locked --no-default-features --features "$AUDIT_CPU_FEATURES" \
  --test terminal_snapshot child_lifecycle -- --test-threads=32
```

The filtered PTY invocation runs the inert lifecycle fixtures; it does not claim to run all
interactive terminal snapshots. The job already sets `NETGET_USE_OLLAMA=0`.

The native Windows resource job adds `process_group_windows_test`. Its fixture uses copies
of the test executable and Windows process handles, without shells, interpreters, network,
or models. It tests Job Object closure killing a child and descendant while an independent
job survives, and abrupt owner termination closing the owner's job without Rust destructors.
A readiness pipe prevents fixture descendants from being created before job assignment.

`pty-process` and `nix` move into Unix-only dev-dependencies; portable terminal parsing
remains in the shared dev-dependencies. This avoids requiring Unix process/PTY APIs when
building the Windows targets. Windows behavior is not inferred from a Unix pass: native
Windows execution remains a distinct result reported by its runner.

Other boundaries remain explicit:

- USB FIDO2 compilation uses the existing native dependency stack; the fixtures never access
  a USB device. They do not establish hardware interoperability or browser WebAuthn behavior.
- The gRPC fixtures use in-memory descriptors and reflection, not `protoc`, grpcurl, a live
  network peer, or a model. Existing interoperability coverage remains separate.
- Unix wrapper parent-death protection uses the existing death-tie mechanism and its documented
  assumptions. Simultaneously killing that independent reaper is outside the mechanism.
- Platform-gated tests excluded on the current host are not counted as runtime passes.
- No general fuzzing, benchmarking campaign, all-protocol runner, or protocol-maturity
  promotion is implied by these focused results.

## 9. Build-script invalidation follows its actual inputs

Previously `build.rs` emitted no explicit change inputs. Cargo therefore used its default
package-directory change detection, causing unrelated test or report edits to rerun the
build script and invalidate the library build.

The script now always emits `cargo:rerun-if-changed=build.rs`. When `etcd` is enabled, it also
tracks the `proto/etcd` directory and the `PROTOC` and `PROTOC_INCLUDE` environment variables,
which are the local schema/compiler inputs used by that conditional protobuf generation.
Non-etcd builds do not add irrelevant protobuf-toolchain watches.

This narrows unnecessary rebuilds without changing the generated code or runtime protocol
behavior. The report does not attach an invented speedup measurement to the change; final
build/lint results and their elapsed times are recorded separately.

## Final integration handoff

All twelve remaining recursion exceptions have implemented code changes and focused tests;
the recursion ratchet now permits no unbounded exceptions. The two wrapper exception
descriptions were corrected and removed, with concrete lifecycle fixes and fixtures added.
The peer-registration ownership hole is fixed through synchronous guard construction.

This report can be appended to the existing consolidated review as the follow-up
implementation record. Final machine-readable verification and publication details should
be appended by the coordinating agent after the remaining checks finish, without replacing
or rewriting the earlier review's historical test evidence.
