# CalDAV tests (and the peers CardDAV shares)

Peers: `python3 tests/server/caldav/install_peers.py ROOT` installs python caldav 3.3.1,
vdirsyncer 0.21.0 and Radicale 3.8.1 (hash-pinned wheels) and prints `NETGET_DAV_BIN`.
`peer.py` drives python caldav. `tests/helpers/dav.rs` (shared with CardDAV) holds the login and
store script (If-Match / If-None-Match, no-uid-conflict), Radicale's launcher and vdirsyncer's
configuration.

- `peer_test.rs` — python caldav: a refused password, discovery, calendars, MKCALENDAR, saves,
  a time-range search returning only the event inside the range, lookup by UID, an update, a
  delete, a duplicate UID it refuses; vdirsyncer: discover and sync a remote event down, then a
  local new event and a local delete up.
- `http_test.rs` — iCalendar validation, unfolding, durations and time-range overlap (all-day,
  recurring); the well-known redirect, 401, principal home-set with a 404 propstat, another
  user's home, Depth 1 listing, PUT with If-None-Match and stale If-Match (412), no-uid-conflict
  (409), invalid data (403), GET with ETag and component, calendar-query by time range and by
  text, multiget with a missing href, an unsupported report, conditional DELETE, MKCALENDAR vs
  MKCOL, OPTIONS; a handler-less server refusing logins and answering 500; the NetGet pair.

`tests/client/caldav/peer_test.rs` — NetGet's client against Radicale.
