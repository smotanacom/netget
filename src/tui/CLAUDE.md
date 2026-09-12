# The dashboard (`src/tui/`) — NetGet Console

The default interactive UI. A full-screen ratatui frame, repainted whole into the alternate
screen. The older rolling-terminal TUI (`src/cli/rolling_tui.rs`) is still behind
`--legacy-tui`; both share `UserCommand::parse`, the status channel, the tick cadences and
the model-selection code.

## The shape of the screen

```
┌ SERVERS 2 · CLIENTS 1 ───────────────────────┐┌ ACTIVITY & CHAT ─────────────────────────┐
│ ● #1  http     :8080      1⇄ ▁▂▅█▅▂ ⚠1 MANUAL││ 02:13:01 http#1  ◆ listening on 127.0.0…  │
│   ⚠ YOUR answer needed · http_request from …  ││ 02:13:04 http#1  ⇄ ⇐ 127.0.0.1:53121 conn…│
│   Running · up 2m13s · ↓1.2K ↑45K · 3 req     ││ 02:13:04 http#1  → :53121 http_request → …│
│   driver MANUAL — you answer each event here  ││ 02:13:09 http#1  ⚠ http_request from :531…│
│   [ stop     ] [ edit     ] [ rules    ]      ││ ▶ start an http server on 8080            │
│   [ driver → LLM ] [ + http client ] [ wire… ]││   Started server #1 (http)                │
│ ▾ peers  1 live · 0 recent                    ││                                           │
│   ● 127.0.0.1:53121 ↓1.2K ↑45K · 3 req ⚠      ││                                           │
│     17:13:23 http_request → send_http_response││                                           │
│ ▸ rules  1 rule · driver MANUAL               ││                                           │
│ ▸ config  2 settings                          ││                                           │
│ ✗ #2  dns      :53 bind 0.0.0.0:53: permissi… ││                                           │
│ ● #3  telnet   →127.0.0.1:2323           LLM  ││                                           │
│   …                                           │├───────────────────────────────────────────┤
│   + new server or client                      ││ > _                                       │
└───────────────────────────────────────────────┘└───────────────────────────────────────────┘
 2 servers · 1 client │ ⚠ 1 waiting for you │ llm: qwen3 │ log:INFO │ F1 keys
```

**Left column = every instance, always visible.** One scrollable canvas of **cards**. A
card is its summary line (status glyph, id, protocol, address, live peers, a 30-second
sparkline, a **driver badge**), the requests parked for you, a facts line (status, uptime,
traffic, rate), the driver, then its **buttons as an aligned grid** — every cell as wide
as the widest label, as many per row as fit — and its **sections**: `peers` (each peer
with its buttons and its requests beneath it), `rules` (each rule with delete / ↑ / ↓
beside it and `+ add rule` below), `config` (each setting opens the form); a client has
`send` (its verbs) and `connections` instead of `peers`. Nothing is selected and nothing
is drilled into: ↑/↓ walk every row, ←/→ walk a row's buttons (and fold / unfold a
section), Enter presses the button or acts on the row, and the letters (`x e r m c n w d`)
act on the card under the cursor. `+ new server or client` at the foot opens one picker
listing both kinds. Rules and config start folded; everything else is open.

**Right column = one stream.** Machine events (instances starting, peers connecting and
closing, every request with the action that answered it, questions parked for you, the
`[LEVEL]` log lines filtered by level) and the conversation (what you typed, the model's
reasoning and replies, command output) are one timeline, newest at the bottom, with the
input box under it. Event lines are one row each; conversation entries wrap. PageUp or
the wheel scrolls back, ↑/↓ then walk lines and Enter opens what one points at.

Tab hops between the two columns. That is the only thing Tab does.

## Manual first, model optional

The dashboard is built around driving instances yourself. Three mechanisms carry that:

- **Driver** (`driver.rs`): a one-word summary of an instance's `*` handler rule —
  `MANUAL` (every unmatched event parks for you), `LLM` (no wildcard rule; the instance
  instruction answers), `SILENT` (`*` → static with no actions: acknowledge, never reply),
  or `RULES` (a wildcard script/static/LLM rule — something custom). The `[ driver → … ]`
  button, and `m` on a card, cycle MANUAL → LLM → SILENT; the non-wildcard rules are kept
  untouched. It applies through `management::update_*` with only `event_handlers` set,
  which is a hot swap — no restart, no dropped connections.
- **Rules section**: the handler table inline. Enter on a rule edits it in the routing
  modal (`modal/routing.rs`); its delete / ↑ / ↓ buttons rebuild the table headlessly
  through `RoutingModel` and apply the same way; `+ add rule` opens the modal on a fresh one.
- **Send section / message a peer**: a client's own verbs as rows (Enter opens the composer
  on that verb's parameters); a server's live peer carries `[ message ]` and
  `[ disconnect ]` on its own row where the protocol registered a peer handle
  (`server/peer_support.rs`). Without a handle the buttons stay, disabled, and say why.

Instances created here default to `*` → manual (see `modal/form.rs`), so the first thing you
see after starting a server and poking it with `curl` is `⚠ … waiting for YOUR answer`, in
the list badge, the inspector overview, the feed, and the status bar.

## Modules

| file | what |
|---|---|
| `mod.rs` | `run_dashboard` entry, model resolution, channel wiring |
| `app.rs` | `DashboardApp`: focus, the canvas cursor, fold state, snapshot, modal stack |
| `event_loop.rs` | terminal lifecycle; the select loop; status-line routing; snapshot absorption |
| `projection.rs` | `AppState` → owned `RailSnapshot` (`ServerRow` / `ClientRow`) under short locks |
| `rail.rs` | the one-line instance summary a card's header renders |
| `cards.rs` | the canvas model: every card's rows, buttons, sections, fold state |
| `driver.rs` | `Driver` detection from a handler table and rebuilding the table for a new driver |
| `activity.rs` | the stream ring (events and conversation), and `Tracker::diff`, which turns two snapshots into events |
| `metrics.rs` | per-instance throughput samples, rates, sparklines, byte formatting |
| `chat.rs` | `route_status_line`: a status line becomes a log entry or a conversation entry |
| `actions.rs` | every instance action (`InstanceAction`) executed in one place |
| `keymap.rs` | global keys, focus cycling, per-pane keys, mouse |
| `modal_keys.rs` | input handling for every modal |
| `hit.rs` | per-frame hit-test registry for the mouse |
| `render/` | one file per pane plus `overlay.rs` for modals |
| `modal/` | forms, composer, routing editor, intercept answer, picker, help, wireshark |
| `wireshark.rs` | protocol → dissector/capture-filter table |

The rule that keeps keyboard and mouse from drifting: an action is an `InstanceAction`, run
by `actions::run`, whatever produced it — a letter, a menu entry, Enter on an item, a click.

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
- `tests/dashboard_rail_test.rs` — the summary line, driver detection/rebuild, and the
  card rows: the button grid, sections, peers with their requests, rules, config, verbs.
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
- 2026-09-12 (second pass): a new instance becomes the selection; the chat hard-wraps its
  rows so the newest line is always visible; `F2` cycles the right column (balanced → chat
  only → feed only); the readline chords (Ctrl-A/E/K/U/W, Alt-word keys) the legacy footer
  had are back in the chat input, taking precedence over the Ctrl-E/Ctrl-W toggles while a
  line is being typed; the pty harness gained `PtyScreen` and renders unpainted cells as
  spaces, and all six snapshots were re-recorded as real screens.

- 2026-09-12 (third pass, from operator feedback): the list + inspector split and the
  popup menu are gone. Every instance is a card that is always open with its buttons (an
  aligned grid) and sections; ↑/↓ walk every row, ←/→ the buttons; Tab only changes column.
  One picker for both kinds. Activity and chat are one stream with the input beneath it.

## Next steps

- Prefer `tests/dashboard_frame_test.rs` (ratatui `TestBackend`, deterministic, populated
  states) for layout assertions. The pty snapshots in `tests/terminal_snapshot/` need
  re-recording after any layout change (delete the `.snap.md`, run, review the new file —
  `assert_snapshot` creates a missing one and passes). Every dashboard pty test waits on
  the text its last key must paint through one `PtyScreen`; never add a fixed sleep.
- Candidates not done: fold-all / unfold-all keys once many cards are open; per-peer
  throughput; an `[ answer all ]` shortcut when several requests are parked.
