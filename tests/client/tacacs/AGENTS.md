# TACACS+ independent client evidence

Use the [server peer bootstrap and pins](../../server/tacacs/AGENTS.md), then
set its printed NETGET_TACACS_PEER/NETGET_TACACS_PYTHON/PYTHONPATH. Both targets
run at100threads; missing peers are required failures, never skips.

The unchanged nwaples/tacplus0.0.3 SDK-backed receiver performs native-client
ASCII/PAP and authorization. Its public accounting callback emits independently
decoded typed request fields before returning SUCCESS; tests assert actual
record flags, ordered arguments, username, NAS port and origin. The receiver
is accurately labeled an SDK-backed peer, not an official daemon. Credential
negatives and credential-free results/redacted injection logs are checked.

Native pair/literal-fake tests supply focused failure/cancellation evidence:
response header session/type/version/sequence/unencrypted mismatches, unsupported
GETDATA abort, RESTART/FOLLOW no retry, safe PASS_ADD/PASS_REPLACE effective
arguments, unknown mandatory denial/optional ignore, no device-policy claim,
atomic invalid-action rejection before TCP, one in-flight operation, parked
connected handler with independent injection, disconnect/removal cancelling
socket/intercepts, common memory and bounded excessive handler actions.
Exact real-wire golden packets are dual-asserted in the server target. Limits,
legacy security exclusions and accounting's non-durability are recorded in
[client scope](../../../src/client/tacacs/AGENTS.md).
