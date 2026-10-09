# CalDAV client — Experimental

The shared client engine (`src/server/dav_common/client.rs`) with CalDAV's flavor. On connect it
discovers the principal from `/.well-known/caldav` (redirects followed by hand, three at most) or
`/`, reads the home-set and lists the home's calendars (`caldav_connected`). Actions: `caldav_list`
(PROPFIND Depth 1), `caldav_get`, `caldav_put` (validated with the server's parser first; `if_match`,
`create_only` → If-None-Match: *), `caldav_delete`, `caldav_query` (calendar-query with an optional UTC time range and component), `caldav_make_collection`
(MKCALENDAR). `caldav_response` carries the status, objects, data and ETag.
