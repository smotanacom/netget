# A2S server (Steam/Source server query)

Hand-written over Tokio UDP (`wire.rs`). Requests: A2S_INFO ('T' with "Source Engine
Query"), A2S_PLAYER ('U') and A2S_RULES ('V').

## Challenges

Players and rules always need a challenge; info needs one only with `info_challenge`. The
challenge is a keyed hash of the client's address with a random per-server key, so nothing
is stored per client and a query from a spoofed address cannot be completed. A missing or
wrong challenge is answered with the right one ('A').

## What the handler decides

`a2s_query {query, remote_addr}` is answered with `a2s_info`, `a2s_players` or `a2s_rules`
(matching the query; any other answer is treated as invalid) or `a2s_refuse`. Rust encodes
the Source formats (protocol 17, EDF port/SteamID/keywords/game id; players with score and
seconds; rules in the handler's order) and splits answers over 1400 bytes into Source split
packets of 1248 bytes.

## Failure modes and bounds

A2S has no error reply, so a refusal, a failure, silence or an invalid answer all leave the
query unanswered (`deliberately_silent`), told apart by their `decision=` tags. Requests are
capped at 1400 bytes, answers at 64 KiB in 64 packets, and 64 queries in flight. No
GoldSource format, A2A_PING or compressed splits.
