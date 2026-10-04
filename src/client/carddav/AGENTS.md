# CardDAV client — Experimental

The shared client engine (`src/server/dav_common/client.rs`) with CardDAV's flavor. On connect it
discovers the principal from `/.well-known/carddav` (redirects followed by hand, three at most) or
`/`, reads the home-set and lists the home's address books (`carddav_connected`). Actions: `carddav_list`
(PROPFIND Depth 1), `carddav_get`, `carddav_put` (validated with the server's parser first; `if_match`,
`create_only` → If-None-Match: *), `carddav_delete`, `carddav_query` (addressbook-query on one property with a text match), `carddav_make_collection`
(extended MKCOL). `carddav_response` carries the status, objects, data and ETag.
