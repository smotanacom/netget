# WAMP client — Experimental

Connects `ws://remote_addr` + `path` offering `wamp.2.json`, sends HELLO for `realm` with the
caller, callee, publisher and subscriber roles (and `authid` when given), and fails unless the
router answers WELCOME (an ABORT's reason is the error). Events: `wamp_welcome`, `wamp_reply`
(the router's answer to each subscribe, unsubscribe, publish, call, register, unregister),
`wamp_event`, `wamp_invocation`, `wamp_left`. Actions: `wamp_subscribe` (exact, prefix,
wildcard), `wamp_unsubscribe` and `wamp_unregister` by name, `wamp_publish`, `wamp_call`,
`wamp_register`, `wamp_yield` / `wamp_error` (the given invocation, or the oldest unanswered),
`wamp_goodbye`, `disconnect`. An invocation the handler leaves unanswered is answered
`wamp.error.unavailable`, so a caller never hangs on NetGet.
