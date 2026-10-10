# Guacamole server tests

No LLM calls: a python policy is the model. `install_peers.py` installs both clients and
prints `NETGET_GUACAMOLE_PYTHON` and `NETGET_GUACAMOLE_JAVA_CP`. The tests fail without them.
- **pyguacamole** 0.11, in a venv.
- **Apache's guacamole-common** 1.5.5 (with slf4j-api), fetched by Maven, with
  `peer/GuacPeer.java` compiled against it. It is the library the Guacamole web
  application uses to speak to guacd.

## `pyguacamole_drives_netget`

`pyguac_session.py` decodes everything itself: PNG dimensions from the IHDR, clipboard text
from the blobs.

The first frame is:
- `size 0 800 600`, as asked;
- a full-screen `rect` with `cfill` `#202060`;
- "NetGet" as a 96×16 PNG (6 glyphs × 8 px × scale 2);
- clipboard "welcome".

Then:
- Typing "hi", BackSpace, "o", Enter reaches the model as "ho", and comes back as a PNG of
  the right width.
- Escape is a key event.
- A click becomes a 10×10 `rect` at the click.
- The clipboard is echoed.
- The model saw no password, and was told one was given.

## `guacamole_common_drives_netget`

`ConfiguredGuacamoleSocket` negotiates VERSION_1_3_0. The test reads the first frame (a PNG
and the clipboard), then types and sends the clipboard. The model is shown the client's
timezone and size. A refused user surfaces as guacamole-common's own
`GuacamoleUnauthorizedException` with the model's message.

## Other tests

- `refusals_and_bounds`: raw instructions.
  - An unreachable model gives exactly `5.error,14.Internal error,3.512;`.
  - A `$id` join is refused.
  - An instruction past 8192 bytes is closed unanswered.
- `framing_counts_code_points`: `Žltá` is 4, and a length that lies is an error.
