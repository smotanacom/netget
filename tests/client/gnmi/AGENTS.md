# gNMI client validation

Client tests require the independently generated public grpcio server pinned
in the receiver test document. The Python server binds port zero itself and
reports the actual readiness port. It implements small deterministic public
SDK RPCs, not a device/YANG datastore. Both library/version and upstream proto
digests are verified before generation; missing peers fail.

`e2e_test.rs` exercises independent Capabilities/Get/Set, identity and gzip,
lossless i64/u64 values and explicit errors, then native ONCE/POLL/STREAM
pairing and cancellation. `bounds_test.rs` checks all four selected encodings,
encoded and gzip-expanded 1 MiB response plus one, 64 KiB JSON plus one,
opaque/nonfinite values, all subscription modes/updates_only, bad sync,
256-response admission, 16 parked calls/cancel/capacity recovery, whole-call
and idle deadlines, client drop with parked manual handlers, verified custom
CA TLS and hostname/untrusted-certificate rejection, and bracketed IPv6.

The initial client run passed seven of nine checks. The two failures were
fixture expectations: invalid TLS is synchronously rejected by management
create(), and tonic's pre-header timeout can report CANCELLED just before the
whole-RPC timer wins. The corrected grpcio test retains the actual status and
verifies elapsed time and owner release. The first raw peer fixture also
withheld headers, so it still triggered that local tonic timer (ten other
client checks passed). The corrected HTTP/2 peer sends successful headers,
then parks the body and ignores grpc-timeout, isolating NetGet's whole-RPC
timer; require DEADLINE_EXCEEDED, RST_STREAM, zero validated messages and a
subsequent successful call on the same connection. Validated-message counters exclude duplicate
sync or the rejected 257th message. Final logs are separate from initial
failures under `.protocol-expansion-20261001/logs/item04-gnmi-*`.

The final whole peer run passed all 11 client and 2 server checks at 100
threads in `item04-gnmi-peers-body-deadline-final.log`. The isolated local
whole-RPC timer ended at 1.011364958 seconds with code 4, zero validated
messages and RST_STREAM CANCEL; the next call on the same connection succeeded.
The public SDK parked-call case ended at 1.010980042 seconds with code 1 and
the idle owner then disconnected. These are separate measured timer paths.

No tests are ignored or skipped for absent peers. Maturity remains
Experimental: these checks do not prove universal target/YANG behavior,
reflection/authentication, unsupported encodings/options, fuzz or pcap.
