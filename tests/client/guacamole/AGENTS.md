# Guacamole client tests

No LLM calls: a python chain is the model. The peers are:
- **guacd** 1.3 with its VNC plugin;
- TigerVNC's **Xvnc**, on a display of the test's own;
- an **xterm** running `cat > file`;
- **xclip**.

Each is spawned by the test, which fails, naming the package, when one is missing.

## `netget_drives_a_vnc_desktop_through_guacd`

The chain, on `guacamole_ready` (800×600, connection id `$…`): click into the xterm, type
"hello netget\n", and set the clipboard.

| Readback | Expected |
|---|---|
| the file | exactly `hello netget\n` |
| `xclip` | "from netget" |

Then:
- xclip sets X's clipboard to "from x". Xvnc sends it to guacd, guacd to NetGet, and the
  chain answers "ack: from x", which `xclip` then reads back.
- An injected Return reaches the file.
- An unknown key name is rejected locally.

## `an_unreachable_vnc_server_is_reported`

guacd's `error` with status 519 (UPSTREAM_NOT_FOUND) reaches the handler as
`guacamole_error`.

Mutation-checked: discarding the model's actions means nothing is ever typed.
