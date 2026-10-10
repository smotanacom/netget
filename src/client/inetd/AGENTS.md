# Classic inetd service clients (RFC 862-868)

Six clients share `mod.rs`. Each query is one exchange, over a fresh TCP connection or one
UDP datagram (`transport`, default `tcp`); `connect` opens nothing, because connecting is
itself a request to Daytime, QOTD, Time and Chargen. Echo and Discard send the action's data
(UTF-8 or hex); Chargen reads `bytes` (default 1024) and checks the RFC 864 pattern; Time
decodes the 32-bit value into Unix seconds and RFC 3339. A UDP query with no reply in 5 s is
logged and raises no event. TCP replies are bounded to 64 KiB and datagrams to 8 KiB.
