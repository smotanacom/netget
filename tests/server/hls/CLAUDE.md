# HLS E2E Tests

## `e2e_test.rs` — `test_hls_playlist_and_segment`

Fetches `/stream.m3u8` then `/seg0.ts` over a real `TcpStream` (what curl/ffplay do at the protocol
level). Asserts:

- playlist → 200, `application/vnd.apple.mpegurl`, `#EXTM3U`, `#EXT-X-TARGETDURATION:6`,
  `#EXTINF:6.000,`, both segment URIs, `#EXT-X-ENDLIST`
- segment → 200, `Content-Type: video/mp2t`, `Content-Length: 4`, and the MPEG-TS sync byte `0x47`
  as the first decoded byte (the segment is supplied `encoding:"hex"` and decoded for real)

Mock: 1 startup + 1 playlist + 1 segment = **3 calls.** Ends with `verify_mocks().await?`.

## `curl_test.rs` — `curl_fetches_playlist_and_segment` (`#[ignore]`)

Real-client validation with `curl`. `#[ignore]` (curl not guaranteed on CI); run manually:

```bash
./cargo-isolated.sh test --no-default-features --features hls \
    --test server -- --ignored --test-threads=1 hls::curl
```

Validated: curl retrieves the m3u8 (`#EXTM3U`, HLS content type, segment URIs) and the segment
(`video/mp2t`). Localhost only.

## `e2e_test.rs` — `hls_connection_stats_are_recorded`

In-process, **zero LLM calls** (a `*` static handler answers). Asserts the one read and one write
land in `update_connection_stats` (`bytes_received`/`bytes_sent`/`packets_*` all > 0) and that HLS
registers **no** peer handle — one-shot HTTP has no live window for `[ message this peer ]`.

## `e2e_test.rs` — the fail-closed pair

Both are in-process and point the LLM at `http://127.0.0.1:1` (a closed port), so no mock and no
Ollama is involved: the failure is real. `http_get_within` gives them a longer read deadline than
the mocked tests, because the answer only comes after the LLM client exhausts its own retries.

- `hls_llm_failure_answers_with_a_category_and_leaks_nothing` — a playlist GET and a segment GET
  with the backend down. Each must get **an answer** (silence is the failure mode: a player hangs
  until its own timeout), status 503 or 500, `Retry-After` present iff 503, body carrying the
  `netget: ` category. `assert_no_internal_leak` then rejects every token that leaked in the real
  incident — `LLM`, `Ollama`, `http://`, `11434`, `/Users/`, `retries`, `✗`, and the internal
  `hex`/`encoding` words the segment path used to echo.
- `hls_empty_segment_action_fails_closed_rather_than_serving_an_empty_200` — a static
  `hls_segment_response` with neither `data` nor `content`. Must not be `200` with
  `Content-Length: 0`; a player would accept that as a valid empty segment and play nothing.

Neither uses the mock server, so neither calls `verify_mocks()`.
