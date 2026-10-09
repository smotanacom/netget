# SRT caller — Experimental

srt-tokio caller with `stream_id`, `latency_ms` and an optional `passphrase`, and the listener's
`announce_mss` (see `src/server/srt/AGENTS.md`). Actions: `srt_receive` (for `seconds`: messages,
bytes, MPEG-TS packets, PIDs and PMT stream types — h264, hevc, aac, mp3 — text messages, and SRT
statistics), `srt_send_file` (an operator-supplied MPEG-TS file of at most 64 MiB, 1316-byte
messages paced at `bitrate_kbps`, then a second for acknowledgements), `srt_send_text`,
`disconnect`. Each operation ends with one `srt_report`; data arriving between operations is
read and discarded.
