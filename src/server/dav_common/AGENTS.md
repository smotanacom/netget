# Shared DAV machinery for CalDAV and CardDAV

Compiled when either `caldav` or `carddav` is enabled.

- `object.rs` — content lines (unfolding, parameters with quoted values, vCard groups),
  components nested and bounded (512 KiB, 20 000 lines, depth 8). `check_calendar`: VCALENDAR
  with VERSION:2.0 and PRODID, one component type (VTIMEZONE aside) sharing one UID.
  `check_vcard`: VERSION 3.0/4.0, FN, UID. `overlaps` implements RFC 4791 §9.9 for DATE and
  DATE-TIME starts with DTEND/DUE/DURATION; floating and TZID times are read as UTC, and a
  recurring component (RRULE/RDATE) counts as running from its first start onward, because
  recurrence sets are not expanded (a query may over-include, never miss). `text_match` is the
  `i;unicode-casemap` collation approximated by Unicode lower-casing.
- `xml.rs` — a namespace-resolved tree over quick-xml (1 MiB, depth 32, 20 000 elements, no
  DOCTYPE); PROPFIND (prop, allprop, propname, empty body) and REPORT (calendar-query,
  calendar-multiget, addressbook-query with limit, addressbook-multiget) bodies; multistatus,
  status responses and DAV error bodies.
- `server.rs` — the server engine; `client.rs` — the client engine; `actions.rs` — the
  per-protocol action and event definitions (`caldav_*`, `carddav_*`).
