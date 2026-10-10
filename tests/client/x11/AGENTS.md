# X11 client tests

No LLM calls; handlers are static and Python scripts.

- `real_server_test.rs` — **Xvfb** (the X.Org server) as the peer, every effect read back with
  `xprop` and `xwininfo`, which are separate clients. `xprop` sets `NETGET_GREETING` on the root;
  the chain reads it, creates a window titled `netget: <value>` watching its structure, then sets
  WM_CLASS (two strings) and a CARDINAL pair and lists the root's children. Asserted from the
  server: the title in `xwininfo -root -tree`, WM_CLASS, _NET_WM_NAME and NETGET_SIZE in `xprop`,
  the size after an injected resize in `xwininfo -id`, and the window gone after disconnect.
  Also: MapNotify reported, BadDrawable for GetGeometry on a bad id, BadWindow for MapWindow (a
  reply-less request, attributed by the sync). The cookie test runs Xvfb with `-auth`: no cookie
  and a wrong one are refused with the server's reason, the right one connects over the Unix
  socket.
- `wire_test.rs` — an X server in the test file: a refused setup surfaces its reason; a reply of
  exactly 1 MiB is read and one 4 bytes larger ends the connection; and against Xvfb, a handler
  answering every result with another action stops at `MAX_FOLLOWUP_DEPTH` (the access log shows
  the 7 results that were answered).

Mutation-checked: dropping the dispatcher's actions, ignoring errors at sync, removing the depth
bound and doubling the reply bound each fail a test.

Xvfb needs `-noreset`: an X server resets when its last client disconnects, so the property
`xprop` set would vanish before NetGet connects. It needs `-nolisten inet6` where there is no
IPv6, and picks no display itself here (`-displayfd` with a chosen free display, retried on a
collision) because `RealServer` treats a parsed port of 0 as "not found".

Peers: `apt-get install xvfb x11-utils xauth`; fails rather than skips without them. CI: the
`x11-client` job in `protocol-pairs.yml`.
