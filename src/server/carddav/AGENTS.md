# CardDAV server — Experimental

The shared engine in `src/server/dav_common/server.rs` with CardDAV's flavor (`actions.rs`).

URL space owned by Rust: `/.well-known/carddav` → 301 to `/` (RFC 6764); `/` and `/principals/`
answer `current-user-principal`; `/principals/<user>/` is the principal; `/addressbooks/<user>/` the
home; `/addressbooks/<user>/<collection>/` a address book; `.../<name>` an object. Path segments are
restricted to `[A-Za-z0-9._-@+~%]`; another user's paths are 403.

- Authentication: HTTP Basic; `carddav_login` decides (only an explicit accept admits; silence and
  failure are 401), and accepted credentials are remembered as a keyed hash for 30 minutes.
  `auth: none` serves `default_user` without credentials.
- PROPFIND (Depth 0/1; prop, allprop, propname; unknown properties in a 404 propstat) answers
  live properties itself: resourcetype, displayname, owner, getetag, getcontenttype,
  getcontentlength, home-set, addressbook-description, supported-address-data, supported-report-set and a
  getctag Rust derives from the members' ETags.
- REPORT: addressbook-query (prop-filter with text-match match types and negate-condition, is-not-defined, anyof/allof on the filter and per prop-filter, limit) and addressbook-multiget, evaluated by Rust over the objects
  the handler lists; other reports are 403 supported-report.
- PUT validates the object (vCard; else 403 valid-address-data) before the handler
  sees it, with the UID, component and If-Match / If-None-Match; GET, DELETE (object or whole
  collection), extended MKCOL (resourcetype must include addressbook, else 403 valid-resourcetype) and PROPPATCH go to the handler. ETags the
  handler omits are a hash of the data.

The handler answers `carddav_request` by operation: list_collections, list_objects, get, put,
delete, make_collection, proppatch (`carddav_collections`, `_objects`, `_object`, `_stored`, `_done`)
or `carddav_error` (status and an optional precondition such as no-uid-conflict). No answer, an
invalid one, or stored data that no longer validates is a 500 with a category message.

Not implemented: sync-collection, vCard version conversion, ACLs, locking. No storage: the handler owns every address book and object.
