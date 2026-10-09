# SCIM service — Experimental, SCIM 2.0 (RFC 7643, RFC 7644)

hyper HTTP/1.1 under `base_path` (default `/scim/v2`), `application/scim+json` (requests may
also be `application/json`). Optional `bearer_token`: every request then needs
`Authorization: Bearer` (constant-time compare, 401 with `WWW-Authenticate: Bearer`).

- `rfc7643_schemas.json` is RFC 7643 §8.7.1 and §8.7.2's own schema representations (User,
  Group, Enterprise User, ServiceProviderConfig, ResourceType, Schema), extracted from the RFC
  text unchanged except for rejoined line wraps. Rust serves `/Schemas`, `/ResourceTypes` (User
  with the Enterprise extension, Group) and `/ServiceProviderConfig` (patch, filter, sort; no
  bulk, etag or changePassword) from them, with `meta.location` rewritten to this service.
  Discovery is read-only (405) and needs no handler. `/Bulk` and `/Me` are 501.
- `query.rs` parses the RFC 7644 §3.4.2.2 filter grammar (attrExp, `pr`, the nine comparison
  operators, and/or/not with precedence, value paths `emails[type eq "work"]`, URN-qualified and
  dotted paths; 4 KiB, depth 16, 128 nodes) and evaluates it with the schema's caseExact
  (a multi-valued attribute compares its `value`s; `ne` on an absent attribute is true), parses
  PATCH paths (`attr`, `attr.sub`, `attr[filter].sub`, a bare extension URN), sorts (absent
  values last) and projects `attributes`/`excludedAttributes` (`returned: always` kept,
  `returned: never` — password — always dropped).

Every operation on Users/Groups raises `scim_request`: list (GET collection, `/.search` on a type
or at the root, which asks once per type), get, create, replace, patch (operations validated:
PatchOp schema, 1..64 ops, add/remove/replace, parsed paths, value rules — refused with
invalidSyntax / invalidPath / noTarget), delete. The handler answers `scim_resources` (Rust then
filters, sorts, pages with startIndex/count ≤ 200 and builds the ListResponse), `scim_resource`
(Rust checks id — it must match for get/replace/patch — puts the core schema in `schemas`, adds
or removes the Enterprise URN with its object, writes `meta`; create → 201 + Location),
`scim_no_content` (delete, patch) or `scim_error` (status + scimType + detail). Client-sent `id`
and `meta` are dropped before the handler sees a resource. No answer, an invalid one or a backend
failure is a 500/503 error with a category message.

No storage: the handler owns every resource.
