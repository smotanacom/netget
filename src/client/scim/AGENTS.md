# SCIM client — Experimental

Reads `ServiceProviderConfig` and `ResourceTypes` at `<scheme>://<remote_addr><base_path>` on
connect (`scim_connected`: resource types and supported features), optional bearer token.
`scim_list` (filter, sortBy, sortOrder, startIndex, count, attributes), `scim_get`,
`scim_create` and `scim_replace` (the core schema URN added when `schemas` is missing),
`scim_patch` (a PatchOp with the given operations) and `scim_delete`; filters and PATCH paths
are checked with the service's own parser before sending. `scim_response` carries the status,
the resource or the ListResponse page and total, or the SCIM error (status, scimType, detail).
