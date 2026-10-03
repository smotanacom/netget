# Graphite emitter validation

`e2e_test` covers exact TCP bytes/counts for structured batches, UTF-8/tagged paths, fractional
timestamps, atomic rejection with next-send recovery, injected disconnect, stop, IPv6 and
Python-script connected handler, manual-handler injection and remote-EOF handle cleanup.

`real_server_test` launches official Carbon 1.1.10 `CarbonReceiverFactory` with its unmodified
`MetricLineReceiver`, using Twisted on an ephemeral loopback port. Its `metricReceived` event
prints structured observations: expected paths, values and timestamps plus actual conversion
of `-1` to receiver time. Carbon's standard instrumentation module is initialized as its
service does. Configuration disables timestamp rounding, flow control and log noise;
no framing/parser/metric conversion methods are replaced. No Whisper/storage service
is created. The peer process has a deadline and kill-on-drop cleanup. Missing Python 3.11
peers fail explicitly; bootstrap is in the server test notes.

Both independent directions cover the selected TCP plaintext scope. UDP/Pickle, a full
Graphite storage service, fuzz execution and pcap evidence remain outside this implementation.
