# StatsD / DogStatsD emitter

Feature `statsd`, registry name `StatsD`, keywords `statsd`/`dogstatsd`, UDP port 8125.
Experimental. No new dependency. Uses the collector's native typed codec.

`dialect` is `dogstatsd` by default, or `statsd` to reject extensions. DNS hostnames and IPv4/
IPv6 socket addresses resolve with Tokio; the local socket matches the first resolved peer's
address family. UDP connection means a destination is configured, not that a collector replied.

`send_statsd_batch` takes `records`, an array of typed metric/event/service_check records.
The entire batch must validate and fit 8192 bytes/256 records before one UDP send happens.
`disconnect` closes the emitter and removes its command handle. The injected command path
returns the actual UDP byte count or `Rejected`; no reply, retransmission or delivery receipt
exists. Sampling is metadata only: the caller decides which observations to send.

Example action:

```json
{"type":"send_statsd_batch","records":[
  {"kind":"metric","name":"requests","value":"1","metric_type":"c","tags":["env:test"]},
  {"kind":"event","title":"Deploy","text":"Started\nCompleted","alert_type":"success"},
  {"kind":"service_check","name":"database","status":0,"message":"available"}
]}
```

The command handle is installed before `statsd_connected`. A single registered task selects
between that handler's future and injected commands, so manual/model waiting does not prevent
emission or disconnect. A disconnect cancels the pending connect handler; stop aborts the task
and drops the only socket. Configured static/script/manual/LLM handlers use the standard client
budget dispatcher. There is no synthetic per-metric sent event and no recursive model loop.

Values are textual to preserve signed gauge deltas and set members. For an absolute negative
StatsD gauge, explicitly send a zero gauge followed by its negative delta. See the collector's
CLAUDE.md for the exact supported DogStatsD subset, validation, and excluded extensions.
Independent receiver tests cover classic StatsD c/g/ms/s; DogStatsD encoding is covered by
published wire vectors and collector interoperability, not an independent Datadog Agent yet.
