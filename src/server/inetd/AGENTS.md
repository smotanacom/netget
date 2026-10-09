# Classic inetd services (RFC 862-868)

Six protocols share one engine in `mod.rs`: Echo (7), Discard (9), Daytime (13), QOTD (17),
Chargen (19) and Time (37). `actions.rs` describes each through `Service` and generates the
protocol types with a macro; `wire.rs` holds the pattern, clock and encoding helpers.

## Transport

`transport` is `tcp`, `udp` or `both` (default): both binds TCP first and then UDP on the
same port number, retrying an OS-assigned port up to 16 times and never replacing an explicit
one. UDP requests keep no connection entry, so nothing needs the connectionless idle sweep.

## What the handler decides

- Echo: each TCP read or UDP datagram (8 KiB at most) raises `echo_request`; `echo_reply`
  without `data` echoes the bytes verbatim, so a static rule gives RFC 862 behaviour with no
  model call. Data is shown as UTF-8 text or hex, with the encoding named.
- Discard: `discard_request` at TCP connect; `discard_reply` keeps the connection (closing
  after `max_bytes` if given) and `discard_refuse` closes it. UDP discard raises nothing.
- Daytime, QOTD, Time: one event per TCP connection or UDP datagram. Daytime and Time fall
  back to the server clock when the answer gives none; Time is sent as RFC 868's 32-bit
  seconds since 1900.
- Chargen: `chargen_request` chooses the character set, line length and byte limit; the
  stream continues until the client closes, a write stalls past 30 s, or the limit.

Every service also offers `<service>_refuse`: deliberately send nothing. It is logged as
`decision=model_reject`, so a refusal is distinguishable from a model that said nothing
(`model_silent`) or failed (`fail_closed_*`), although all three put the same nothing on the
wire.

## Failure modes

No failure produces output the handler did not give: TCP connections are closed (Echo
half-closes), UDP requests go unanswered. Every outcome is logged with `decision=`.

## Bounds

8 KiB per read or datagram (oversized datagrams dropped), the shared accept cap,
`idle_timeout_secs` for Echo and Discard, 30 s writes, 64 UDP requests in flight, quotes
capped at 512 characters and daytime text at 256.
