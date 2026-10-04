# RESTCONF client — Experimental

Over the shared HTTP fetch client. Discovers the root through `/.well-known/host-meta` (XRD XML
or JSON; `/restconf` otherwise), reads the API root and yang-library-version when the server
serves them (FreeCONF does not, so their status is reported rather than required), and the
modules from `ietf-yang-library:modules-state` (wrapped or not). Reports `restconf_connected`.
Actions: get (depth, content, fields), put, post, patch, delete, invoke (input wrapped as
`module:input`); each answer is `restconf_response` with status, data, RFC 8040 errors and
Location. Paths are checked with the server's parser. Optional HTTP Basic credentials. Plain
HTTP and JSON only.
