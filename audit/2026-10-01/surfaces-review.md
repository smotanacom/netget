# Surface review — 2026-10-01

## Scope and operating constraints

Reviewed the native dashboard, shared UI state, CPU raster display implementation, browser shim crates, browser demo/test infrastructure, landing page, and npm launcher. No GPU, WebGPU adapter, model download, LLM inference, remote model endpoint, deployment, publication, or commit was used. Tests are CPU-only and either pure functions, in-memory transport, a synthetic terminal, or a harmless fixture child process.

This is an evidence-based improvement pass, not a claim of complete formal verification. Every source area below was inventoried and scanned; detailed behavioral inspection concentrated on input/rendering, raster bounds, browser timer/network semantics, browser answer conversion, streamed Telnet traffic, and executable resolution. Unchanged modules received interface/risk-pattern review, not exhaustive execution of every branch. The root coordinator owns whole-project Rust validation and the final consolidated report.

## Implemented changes

### Native dashboard

1. **Keep long and multiline input visible.** The chat box previously rendered from the first line and column forever, even when editing the ninth line in a five-row box. The cursor disappeared past the right edge. It now selects a vertical viewport around the cursor and clips the horizontal viewport at grapheme boundaries. The prompt remains visible.
2. **Place input cursors in terminal cells.** InputState uses Unicode scalar indices, whereas wide CJK symbols occupy two cells and combining marks occupy none. Cursor placement now measures the prefix with ratatui's display-width rules. Horizontal clipping pads a partially clipped wide glyph and avoids splitting a grapheme.
3. **Avoid input-height truncation.** Input line counts are bounded before converting to u16; large pastes can no longer wrap the height through a narrowing cast.
4. **Wrap conversation text by display width.** Character-count chunking used to silently clip the latter half of wide-character lines. Conversations now wrap whole graphemes before the terminal width is exceeded; combining sequences stay intact.
5. **Preserve trailing newlines in text editors.** Opening and accepting an existing script/instruction previously removed trailing line endings because str::lines omits the final empty line. Splitting on newline preserves it, including repeated blank lines and CRLF content.
6. **Restore raw mode after terminal setup failures.** The terminal drop guard is now armed immediately after entering raw mode, before alternate-screen/mouse-capture setup can fail.

Native regressions were added to `tests/dashboard_frame_test.rs` for long/multiline input, wide and combining cursor placement, trailing newlines, and CJK conversation wrapping.

### CPU display rendering

7. **Allow zero-dimension canvases.** Empty canvas dimensions return an empty image with the requested dimensions rather than panicking while creating a pixmap.
8. **Treat degenerate shapes as no-ops.** Zero-width/height rectangles, zero-radius circles, and any path rejected by tiny-skia no longer unwrap a missing path/rectangle.
9. **Prevent coordinate overflow.** Window offsets, nested commands, textbox origins and button-label coordinates use saturating arithmetic. Offscreen coordinates remain offscreen instead of wrapping into the canvas or panicking in debug builds.
10. **Reject invisible text early.** Zero-size fonts, fully transparent text, empty strings and offscreen origins return without shaping invalid or unnecessary text.
11. **Render multiline text and color glyphs correctly.** The renderer now uses cosmic-text's pixel callback, which applies each line's baseline and distinguishes grayscale masks from RGBA glyph images. The old loop treated each RGBA byte as another alpha pixel and placed every line at the same baseline.
12. **Respect text alpha and premultiplied destination alpha.** Source-over compositing applies both glyph coverage and requested color alpha; transparent pixmaps retain meaningful alpha instead of every touched pixel becoming opaque.
13. **Reuse glyph raster cache across calls to a TextRenderer.** SwashCache now belongs to the renderer instead of being recreated for each draw.
14. **Make ASCII art actually monospace.** AsciiRenderer requests the monospace family, bounds row coordinate arithmetic, and stops once subsequent rows are outside the image.
15. **Center button labels by Unicode scalar count rather than UTF-8 byte count.** This retains the existing approximate seven-pixel metric while removing a systematic non-ASCII bias.

New `tests/display_rendering_test.rs` covers empty surfaces, zero shapes, offscreen nested windows, transparent/half-transparent text, multiline baselines, and invalid font/origin inputs. This is software rasterization, not GPU rendering. The positive text fixture requires usable system fonts and fails rather than silently skipping if none render.

### Browser tokio compatibility

16. **Respect signed host timer limits.** The locally installed gloo-timers implementation casts its u32 argument to i32. Delays over i32::MAX milliseconds previously became negative host timers and could repeatedly wake immediately. Each host timer chunk now clamps to i32::MAX.
17. **Preserve full requested deadlines.** Only host timer chunks are clamped; sleep/timeout deadlines no longer get shortened to the old maximum. An overflowing deadline saturates to the representable maximum instead of completing immediately.
18. **Round fractional milliseconds upward.** Positive sub-millisecond delays no longer become zero-delay timer churn.
19. **Skip missed interval ticks in constant time.** The previous loop stepped once per missed period; a suspended browser could do millions/billions of iterations. Modular duration arithmetic finds the next original schedule boundary directly.
20. **Make UDP try_recv_from read the actual queue.** It previously saw only a datagram buffered by readable/peek; data already queued by a sender still returned WouldBlock. The method now checks peeked data first and then tries the receiver without awaiting.
21. **Align connected try_recv with async recv.** Non-connected use reports NotConnected; packets from other peers are discarded until the connected peer's datagram is found or the queue is empty.

Pure timer math is in a small internal module shared by native regression tests through a path inclusion; this does not add testing code under src. UDP tests use in-memory channels, with no real network or browser.

### Browser demo and answer composer

22. **Parse Telnet incrementally across TCP chunks.** A connection-scoped decoder retains IAC, option negotiation and subnegotiation state. Split command bytes are no longer lost or mistaken for visible text, and a missing option byte cannot generate a reply for invented option zero.
23. **Decode UTF-8 incrementally.** A connection-local streaming TextDecoder retains multibyte characters split across network callbacks. It flushes any remaining decoder state at close.
24. **Normalize split CRLF correctly.** Newline normalization remembers whether the preceding callback ended with CR, avoiding duplicate carriage returns.
25. **Preserve raw answer edits.** Clicking the already active raw tab no longer overwrites edits with the old form. Moving to Form is refused when conversion would change the JSON; the visitor keeps the raw text and receives an explanation.
26. **Reject lossy envelope conversion.** Raw-to-form conversion compares rebuilt JSON structurally, preserves property ordering independence, and rejects null/non-array action lists, removed optional values, missing required fields, and tool/action category changes that the form cannot preserve.
27. **Preserve literal __proto__ fields as data.** Form action and extras dictionaries use null prototypes, so JSON metadata keys do not invoke Object.prototype setters or disappear during serialization.
28. **Reject unsafe numeric integers in form fields.** Values beyond JavaScript's exact integer range now produce a field error directing the visitor to Raw JSON instead of silently rounding the wire value.

`web/test/telnet_test.mjs` tests every two-chunk split of a mixed negotiation/Unicode/subnegotiation stream, one-byte chunks, empty chunks and incomplete option negotiation. `web/test/composer_model_test.mjs` tests precise integers, faithful conversion and prototype-named JSON properties. These can run without a WASM bundle or browser.

### npm launcher

29. **Separate downloaded caches by platform/architecture/libc.** The old cache used only package version, so Rosetta/native Node or a shared cache could execute the wrong architecture. Cache paths include the resolved platform key.
30. **Stream archive downloads with a deadline.** Downloading previously buffered the entire archive in memory and had no deadline. The launcher now pipes the response to disk and applies a two-minute abort signal to the fetch and stream.
31. **Reject extracted non-regular binary files.** A symlink/directory named netget no longer passes the binary check.
32. **Remove failed staging files.** The cache staging file is cleaned even if chmod/rename fails.
33. **Forward repeated termination signals while the child lives.** child.killed only means a signal was sent, not that the child exited. Signal forwarding now checks actual exit/signal status so a second termination attempt is not discarded.

`tests/npm_launcher_test.cjs` uses a harmless Node/shell child to assert argv/stdout/exit propagation and platform-keyed cache selection. It never runs NetGet or downloads anything. The cache fixture is Unix-specific and would skip on Windows; both tests ran here without skips.

## Verification completed by this agent

- `env CARGO_TARGET_DIR=/private/tmp/netget-audit-wasm-20261001 RUSTC_WRAPPER= CARGO_PROFILE_TEST_DEBUG=0 cargo test -p netget-tokio-wasm --test tcp_peek_test --test udp_receive_test --test time_math_test --locked --offline`: **14 passed**, 0 failed, 0 ignored. First build took 2m29s; actual tests completed without waiting.
- `node --test web/test/composer_model_test.mjs web/test/telnet_test.mjs`: **6 passed**, 0 failed, 0 skipped.
- `node --test tests/npm_launcher_test.cjs`: **2 passed**, 0 failed, 0 skipped.
- `node --check site/js/demo.js`, `node --check site/js/composer.js`: passed.
- `bash -n site/deploy.sh web/build.sh`: passed; deployment/build scripts were syntax checked, not executed.
- Targeted `rustfmt --edition 2021` and `git diff --check`: passed at this report checkpoint.
- Root Rust dashboard/display regression execution is coordinated by the root agent; its final result must be read from the consolidated report. No success is inferred from formatting alone.

One newly written composer test initially incorrectly expected a JSON string number to be rejected. Inspection showed that source strings become text fields and round-trip exactly; the test was corrected to require preservation. This did not relax existing repository expectations.

## Limits and deferred findings

- No GPU-backed demo, real browser model, real LLM endpoint, or GPU capability probe was run. Browser model management paths were inspected statically only.
- The existing full browser/Playwright smoke suites were not executed in this section because they can exercise GPU/model availability paths and require a built WASM bundle. New parser/model suites are independent and CPU-only.
- The new site module is inside versioned js assets, so the existing deployment packaging picks it up. No site deployment occurred; changes are local and reviewable.
- Font discovery still occurs when a new TextRenderer is created for each canvas text command; a render-scoped font context would avoid repeated discovery but requires wider ownership changes. The glyph cache is reused within each existing renderer.
- DisplayCanvas still has an infallible public render API for nonempty allocation failure; huge allocation requests need a deliberate fallible API/resource-limit design. Deep nested window trees still use recursive execution and clone nested commands while offsetting.
- Virtual UDP queues and TCP listener accept queues remain unbounded; explicit backpressure/loss policy should be designed before changing their semantics. Concurrent UDP peek/readiness interactions and recv_from filtering on connected sockets remain candidates for stronger parity tests.
- ActivityFeed cursor/scroll preservation when the bounded ring evicts old entries, and pruning per-card fold state after instances vanish, deserve further lifecycle tests.
- Some inactive legacy src/ui layout/event files still reference removed UI APIs; src/ui/mod.rs does not compile them. This pass did not remove historical files opportunistically.
- The npm fallback still trusts the release transport/artifact source and system archive extractor; it does not verify a release checksum/signature. Release manifest provenance should be implemented together with the packaging pipeline. Download timeout/staging failure and repeated-signal behavior were source reviewed but not fault-injection tested here.
- Windows archive extraction, cache replacement behavior and signal propagation were not executed on this macOS host.
- Existing docs carry historical claims that can drift (public/private repository state, protocol counts, feature availability). No external services were queried to refresh those claims.

## File inventory covered

The list below includes source, manifests, static assets, scripts, documentation and browser tests in the owned roots (generated demo/pkg, pycache and other build products excluded). New files from this pass are included. Detailed inspection covered the changed areas and their immediate callers; the remaining entries received inventory/interface/risk scanning, not a guarantee of branch-complete review.

- `src/tui/CLAUDE.md`
- `src/tui/actions.rs`
- `src/tui/activity.rs`
- `src/tui/app.rs`
- `src/tui/cards.rs`
- `src/tui/chat.rs`
- `src/tui/command_exec.rs`
- `src/tui/commands.rs`
- `src/tui/driver.rs`
- `src/tui/event_loop.rs`
- `src/tui/hit.rs`
- `src/tui/keymap.rs`
- `src/tui/metrics.rs`
- `src/tui/mod.rs`
- `src/tui/modal/composer.rs`
- `src/tui/modal/confirm.rs`
- `src/tui/modal/form.rs`
- `src/tui/modal/help.rs`
- `src/tui/modal/intercept.rs`
- `src/tui/modal/mod.rs`
- `src/tui/modal/protocol_picker.rs`
- `src/tui/modal/request_detail.rs`
- `src/tui/modal/routing.rs`
- `src/tui/modal/text_editor.rs`
- `src/tui/modal_keys.rs`
- `src/tui/projection.rs`
- `src/tui/rail.rs`
- `src/tui/render/cards.rs`
- `src/tui/render/chat.rs`
- `src/tui/render/mod.rs`
- `src/tui/render/overlay.rs`
- `src/tui/render/rail.rs`
- `src/tui/render/status_bar.rs`
- `src/tui/render/stream.rs`
- `src/tui/theme.rs`
- `src/tui/uimsg.rs`
- `src/tui/wireshark.rs`
- `src/ui/app.rs`
- `src/ui/events.rs`
- `src/ui/layout.rs`
- `src/ui/mod.rs`
- `src/display/ascii.rs`
- `src/display/canvas.rs`
- `src/display/mod.rs`
- `src/display/text.rs`
- `src/display/types.rs`
- `crates/netget-crossterm-wasm/Cargo.toml`
- `crates/netget-crossterm-wasm/src/lib.rs`
- `crates/netget-tokio-wasm/Cargo.toml`
- `crates/netget-tokio-wasm/src/fs.rs`
- `crates/netget-tokio-wasm/src/lib.rs`
- `crates/netget-tokio-wasm/src/net.rs`
- `crates/netget-tokio-wasm/src/process.rs`
- `crates/netget-tokio-wasm/src/runtime.rs`
- `crates/netget-tokio-wasm/src/signal.rs`
- `crates/netget-tokio-wasm/src/task.rs`
- `crates/netget-tokio-wasm/src/time.rs`
- `crates/netget-tokio-wasm/src/time_math.rs`
- `crates/netget-tokio-wasm/tests/tcp_peek_test.rs`
- `crates/netget-tokio-wasm/tests/time_math_test.rs`
- `crates/netget-tokio-wasm/tests/udp_receive_test.rs`
- `crates/netget-web/Cargo.toml`
- `crates/netget-web/src/backend.rs`
- `crates/netget-web/src/input.rs`
- `crates/netget-web/src/lib.rs`
- `web/README.md`
- `web/build.sh`
- `web/test/composer_model_test.mjs`
- `web/test/page_composer.py`
- `web/test/smoke.mjs`
- `web/test/telnet_test.mjs`
- `site/CLAUDE.md`
- `site/css/demo.css`
- `site/css/style.css`
- `site/deploy.sh`
- `site/favicon.svg`
- `site/index.html`
- `site/js/composer.js`
- `site/js/demo.js`
- `site/js/main.js`
- `site/js/telnet.js`
- `site/js/thinking.js`
- `npm/netget/README.md`
- `npm/netget/bin/netget.js`
- `npm/netget/package.json`
