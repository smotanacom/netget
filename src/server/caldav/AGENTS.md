# CalDAV server — Experimental

The shared engine in `src/server/dav_common/server.rs` with CalDAV's flavor (`actions.rs`).

URL space owned by Rust: `/.well-known/caldav` → 301 to `/` (RFC 6764); `/` and `/principals/`
answer `current-user-principal`; `/principals/<user>/` is the principal; `/calendars/<user>/` the
home; `/calendars/<user>/<collection>/` a calendar; `.../<name>` an object. Path segments are
restricted to `[A-Za-z0-9._-@+~%]`; another user's paths are 403.

- Authentication: HTTP Basic; `caldav_login` decides (only an explicit accept admits; silence and
  failure are 401), and accepted credentials are remembered as a keyed hash for 30 minutes.
  `auth: none` serves `default_user` without credentials.
- PROPFIND (Depth 0/1; prop, allprop, propname; unknown properties in a 404 propstat) answers
  live properties itself: resourcetype, displayname, owner, getetag, getcontenttype,
  getcontentlength, home-set, calendar-description, supported-calendar-component-set, supported-calendar-data, calendar-color, calendar-user-address-set, supported-report-set and a
  getctag Rust derives from the members' ETags.
- REPORT: calendar-query (comp-filter, time-range, prop-filter, text-match, is-not-defined) and calendar-multiget, evaluated by Rust over the objects
  the handler lists; other reports are 403 supported-report.
- PUT validates the object (iCalendar; else 403 valid-calendar-data) before the handler
  sees it, with the UID, component and If-Match / If-None-Match; GET, DELETE (object or whole
  collection), MKCALENDAR and PROPPATCH go to the handler. ETags the
  handler omits are a hash of the data.

The handler answers `caldav_request` by operation: list_collections, list_objects, get, put,
delete, make_collection, proppatch (`caldav_collections`, `_objects`, `_object`, `_stored`, `_done`)
or `caldav_error` (status and an optional precondition such as no-uid-conflict). No answer, an
invalid one, or stored data that no longer validates is a 500 with a category message.

Not implemented: sync-collection, scheduling (RFC 6638), free-busy, recurrence expansion, ACLs, locking. No storage: the handler owns every calendar and object.
