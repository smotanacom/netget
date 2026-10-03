# Selected legacy TACACS+ client

Feature/registry/transport scope matches the
[server](../../server/tacacs/AGENTS.md). Experimental legacy RFC8907 only;
RFC9887 TLS1.3, mutual certificates and port300 are excluded. Startup
shared_secret stays private in transport settings; cloned ConnectContext
startup fields are cleared before handlers run. Optional DNS/connect/packet
and event-handler deadlines default30s and allow1..300s; writes10s.

Logical readiness confirms name resolution, not remote service availability.
Every authenticate_tacacs, authorize_tacacs or account_tacacs operation opens a
fresh TCP session, with cryptographic random32bit session ID and no replay or
multiplexing. Response version/type/session/sequence/obfuscation flag is checked
before publishing any result. Client events omit password/transport secret;
password is an explicit top-level action parameter so shared model and injected
log privacy recognizes it. Only typed context/arguments/status/messages are
model-facing; legacy wire bytes remain internal.

ASCII LOGIN answers GETUSER/GETPASS within8rounds; unsupported GETDATA or prompt
exhaustion sends an abort and fails. PAP LOGIN minor1 is a single request/reply.
RESTART/FOLLOW are treated as FAIL without retry, redirects or local fallback.
The selected printable ASCII subset and non-space usernames exclude full
PRECIS/Unicode and other authentication flows.

PASS_ADD appends reply arguments to request arguments; PASS_REPLACE replaces
requests. Ordered duplicates remain intact. Unknown mandatory effective
arguments deny authorization; optional unknown arguments are reported and
ignored. The default handled names are service/cmd/cmd-arg/priv-lvl, configurable
up to32names. priv-lvl must parse0..15. Results report decisions and effective
arguments without applying device policy. Accounting SUCCESS is only the peer's
recording assertion; durable_storage_confirmed is always false.

One exchange is in flight. The bounded command channel accepts injections
independently of a parked connected/result handler; another operation is
rejected while one runs. Disconnect and removal cancel exchange/handlers and
clear the command handle. The caller's injection timeout alone does not cancel
transport. At most32queued events/actions and follow-up depth8 are allowed;
actions are iteratively preflighted before copying and rejected JSON is dropped
iteratively. Wire-body/argument/string limits match the server; PASS_ADD may
combine64arguments. Common instance memory/handlers/access logging remain
shared; there is no protocol account or policy store.

[Tests](../../../tests/client/tacacs/AGENTS.md) require an unchanged independent
SDK-backed server. No production capture or fuzz claim is made.
