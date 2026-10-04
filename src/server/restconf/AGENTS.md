# RESTCONF server — Experimental

RFC 8040 over hyper HTTP/1.1, JSON only (`application/yang-data+json`). `path.rs` parses data
resource identifiers (module-qualified first segment, `=key1,key2` with percent-decoding, 64
segments); `module:` alone addresses a module's top-level nodes, which is how FreeCONF names a
module without a top container.

Rust owns:
- Discovery: `/.well-known/host-meta` as XRD XML, or JSON when asked; the API root
  (`/restconf`: data, operations, yang-library-version) and `/restconf/yang-library-version`.
- The YANG library: `ietf-yang-library:modules-state` (and its `module` and `module-set-id`)
  generated from the `modules` startup parameter; `/restconf/operations` from `operations`.
- Media types: an Accept that excludes JSON is 406; a body that is not YANG JSON is 415, or 400
  `malformed-message` when it does not parse; 1 MiB bodies (413).
- Query parameters depth, content, fields, with-defaults, insert and point are checked and
  passed on; anything else is 400 `invalid-value`.
- Methods: GET, HEAD (no body), POST (201 with Location), PUT, PATCH, DELETE (204), OPTIONS
  (Allow); replacing or deleting the whole datastore is 405.
- Errors as RFC 8040 error documents, the status defaulting from the error-tag.

The handler is the datastore: `restconf_data_request` (method, path, parsed target, query,
body) answered by `restconf_data` (RFC 7951 JSON written by the handler — top-level members
module-qualified, a list entry wrapped as `{"m:list":[…]}`), `restconf_ok` or `restconf_error`;
`restconf_operation` (the `module:input` wrapper removed) answered by `restconf_output` (wrapped
as `module:output`; none is 204) or `restconf_error`. No answer is 500 operation-failed (503 when
overloaded). No YANG parsing or validation: declared modules are listed, not checked.

Not implemented: XML, event streams, ETag/Last-Modified, NMDA datastores, YANG Patch.
