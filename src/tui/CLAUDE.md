# The dashboard (`src/tui/`) — NetGet Console

The default interactive UI. A full-screen ratatui frame, repainted whole into the alternate
screen. The older rolling-terminal TUI (`src/cli/rolling_tui.rs`) is still behind
`--legacy-tui`; both share `UserCommand::parse`, the status channel, the tick cadences and
the model-selection code.

## The shape of the screen

```
┌ SERVERS 2 · CLIENTS 1 ──────────────┐┌ ACTIVITY ───────────────────────────────┐
│ ● #1 http    :8080   2⇄ ▁▂▅█▅▂ MANUAL│ 02:13:01 http#1  ⇐ 127.0.0.1:5321       │
│ ● #2 tcp     :9000   0⇄        SILENT│ 02:13:01 http#1  http_request → send_…  │
│   + new server                       │ 02:13:05 dns#3   ✗ bind failed (EADDRIN)│
│ ● #4 telnet  →127.0.0.1:2323     LLM │                                          │
│   + new client                       │                                          │
├ #1 http :8080 ───────────────────────┤├ CHAT ────────────────────────────────────┤
│ overview│peers│traffic│rules│config  │ ▶ start an http server on 8080           │
│ [ stop ] [ edit ] [ rules ] [ + client│   Started server #1 (http)               │
│ status   Running · up 2m 13s         │├──────────────────────────────────────────┤
│ …                                    ││ > _                                      │
└──────────────────────────────────────┘└──────────────────────────────────────────┘
 2 servers · 1 client │ ⚠ 1 waiting │ llm: qwen3 │ log:INFO │ F1 keys
```

**Left column = management.** The **instance list** on top is one line per server and
client: status glyph, id, protocol, address, live peer count, a 30-second throughput
sparkline, and a **driver badge** saying who answers this instance's events. Under each
section sits its `+ new …` row, where the instance it creates will appear. Below the list,
the **inspector** shows the selected instance in tabs — `overview`, `peers` (for a client:
`connections`), `traffic`, `rules`, `config`, and for clients `send` — each with its own
**action bar** of buttons (Tab stops, clickable). The inspector never grows the list: a busy
instance scrolls inside its own pane.

**Right column = what is happening.** The **activity feed** is the machine's view: instances
starting/stopping, peers connecting/closing, every request with the action that answered it
(Enter opens the full request/response), questions parked for you (Enter answers), and the
`[LEVEL]` log lines the status channel carries, filtered by log level. The **chat** below is
the conversation: what you typed, the model's reasoning and replies, command output, and
errors. The chat pane sizes itself to its content (up to half the column), so a session that
never talks to a model gives the feed the whole column.

Why two panes rather than one stream: before this split, `[INFO]` lines from every server
interleaved with the conversation, and a question you asked the model scrolled off under
its own tool-call logging within seconds.

## Manual first, model optional

The dashboard is built around driving instances yourself. Three mechanisms carry that:

- **Driver** (`driver.rs`): a one-word summary of an instance's `*` handler rule —
  `MANUAL` (every unmatched event parks for you), `LLM` (no wildcard rule; the instance
  instruction answers), `SILENT` (`*` → static with no actions: acknowledge, never reply),
  or `RULES` (a wildcard script/static/LLM rule — something custom). `[ driver: … ]` in the
  action bar, and `m` on an instance, cycle MANUAL → LLM → SILENT; the non-wildcard rules are
  kept untouched. It applies through `management::update_*` with only `event_handlers` set,
  which is a hot swap — no restart, no dropped connections.
- **Rules tab**: the handler table inline, with add / edit / delete / move up / move down in
  the bar. Add and edit open the routing modal (`modal/routing.rs`); delete and move rebuild
  the table headlessly through `RoutingModel` and apply the same way.
- **Send tab / message a peer**: a client's own verbs as rows (Enter opens the composer on
  that verb's parameters); a server's live peer gets `[ message ]` and `[ disconnect ]` where
  the protocol registered a peer handle (`server/peer_support.rs`).

Instances created here default to `*` → manual (see `modal/form.rs`), so the first thing you
see after starting a server and poking it with `curl` is `⚠ … waiting for YOUR answer`, in
the list badge, the inspector overview, the feed, and the status bar.

## Modules

| file | what |
|---|---|
| `mod.rs` | `run_dashboard` entry, model resolution, channel wiring |
| `app.rs` | `DashboardApp`: focus, selection, per-pane UI state, snapshot, modal stack |
| `event_loop.rs` | terminal lifecycle; the select loop; status-line routing; snapshot absorption |
| `projection.rs` | `AppState` → owned `RailSnapshot` (`ServerRow` / `ClientRow`) under short locks |
| `rail.rs` | the instance list model: rows, per-instance line, status glyphs |
| `inspector.rs` | tabs, action bar, per-tab lines and selectable items |
| `driver.rs` | `Driver` detection from a handler table and rebuilding the table for a new driver |
| `activity.rs` | the feed ring, and `Tracker::diff`, which turns two snapshots into events |
| `metrics.rs` | per-instance throughput samples, rates, sparklines, byte formatting |
| `chat.rs` | the conversation ring; `route_status_line` decides feed vs chat |
| `actions.rs` | every instance action (`InstanceAction`) executed in one place |
| `keymap.rs` | global keys, focus cycling, per-pane keys, mouse |
| `modal_keys.rs` | input handling for every modal |
| `hit.rs` | per-frame hit-test registry for the mouse |
| `render/` | one file per pane plus `overlay.rs` for modals |
| `modal/` | forms, composer, routing editor, intercept answer, picker, help, wireshark |
| `wireshark.rs` | protocol → dissector/capture-filter table |

The rule that keeps keyboard and mouse from drifting: an action is an `InstanceAction`, run
by `actions::run`, whatever produced it — a letter, Enter on a bar button, a click on it.

## Things that must not run on the event loop

Creating a server, connecting a client, sending through one, applying a routing change: all
network I/O, all spawned, all reporting back through `uimsg::UiMsg`. Awaiting one inline
froze the whole dashboard for the kernel's SYN-retry window once. `handle_ui_msg` closes the
originating modal on success and leaves it open showing the error on failure.

## Tests

- `tests/dashboard_frame_test.rs` — whole frames rendered into ratatui's `TestBackend`:
  the empty dashboard at 80×24, a populated one (peers, traffic, a parked request, an
  errored server, a client with verbs), every tab for both kinds at the minimum size, and
  what the selection does when its instance vanishes. **Start here for any layout change**;
  `--nocapture` prints the frames.
- `tests/dashboard_rail_test.rs` — list rows, instance lines, driver detection/rebuild,
  inspector items and bar buttons per state.
- `tests/dashboard_activity_test.rs` — `Tracker::diff` emits each lifecycle/peer/request/
  waiting event exactly once; status-line routing.
- `tests/dashboard_routing_test.rs`, `dashboard_create_flow_test.rs`,
  `dashboard_wireshark_test.rs` — the modals and the create path.
- `tests/terminal_snapshot/` — the real binary in a pty: an 80×24 snapshot of the first
  frame, and `test_dashboard_starts_and_stops_a_server_from_the_keyboard`, which drives
  Tab → `a` → `tcp` → Enter → Enter → `x` with no model configured and waits on what each
  key must paint. `assert_snapshot` creates a missing snapshot and passes, so review a new
  one before trusting it.

## Progress log

- 2026-09-12: layout swapped (management left, activity + chat right); instance list +
  tabbed inspector replace the single tree; activity feed derived from snapshot diffs;
  driver badge/cycle; throughput sparklines; status bar reworked.

## Next steps

- Prefer `tests/dashboard_frame_test.rs` (ratatui `TestBackend`, deterministic, populated
  states) for layout assertions. The pty snapshots in `tests/terminal_snapshot/` need
  regenerating after any layout change (see that file's header); review the `.actual.snap.md`
  before promoting. **Five of the six pty snapshots are blank captures with only the typed
  text on the last row** (`typed_simple_input`, `cursor_navigation`, `ctrl_k_delete`,
  `input_line`, `usage_command_enabled`): `capture_screen` builds a fresh vt100 parser from
  only the bytes read in that call, so a second capture after the first drained the frame
  sees only the diff — and a cell that happened to hold the same character in the previous
  frame is never re-emitted, so even the text that *is* captured can be garbled ("listein").
  They were like that before the redesign and are byte-identical after it. The harness now
  has `PtyScreen` (one parser fed for the whole test), which the keyboard-driven test uses;
  moving those five tests onto it and re-recording them is worth a pass of its own.
- Candidates not done: a `/` filter on the instance list once it grows past a screen; a
  keyboard toggle to maximise the chat pane; per-peer throughput; an `[ answer all ]`
  shortcut when several requests are parked.
