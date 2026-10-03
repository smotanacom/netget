# Diameter client validation

Run the required unchanged independent receivers described in the
[server test guide](../../server/diameter/AGENTS.md). Both python-diameter0.9.0
Node and fiorix/go-diameter4.5.0 public state-machine roles must be present;
missing peers fail the suite. Healthy owned peer shutdown must exit0.

Client tests verify CER/CEA before readiness, all three selected stateless NASREQ
request types, accepted and rejected PAP with typed result readback, password
omission from result events and injected-action log redaction. Invalid capability
answers leave no registered client or connected event. Invalid success answers
must fail correlation, mandatory-field or stateless agreement checks before any
accepted result event.

Lifecycle tests cover injection while a connected handler is parked, one pending
AAA operation, real watchdog/disconnect controls independent of handlers,
constructed10000-depth action refusal with a valid control afterward, deadlines,
disconnect/removal cancellation and absence of replay. Native pair tests read
shared connection counters and common access records. Keep socket tests under
the programme's serialized build/disk guard; no Cargo dependency or standalone
target is added for peers. These checks do not establish full RFC compliance,
secure transport, fuzz coverage or production-capture maturity.
