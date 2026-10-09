# RDAP client — Experimental

Queries one server: `http(s)://<remote_addr><base_path>/`. `rdap_query` is built by the
server's own RFC 9082 parser (`src/server/rdap/query.rs`), so the client cannot send a query
the server would refuse as malformed, and values are normalized identically. Requests ask
for `application/rdap+json`. reqwest is built **without redirect following**
(`configured_for_endpoint` keeps the loopback DNS/proxy bypass): a 3xx is reported as
`redirect` with its Location, never followed, because RDAP referrals point at other operators.

Each answer becomes `rdap_response`: a 200 must be RDAP JSON with `rdapConformance` and the
objectClassName (or `*SearchResults` member) the query asked for; 4xx/5xx carry the error
object when the server sent one. A transport failure or a malformed answer is logged and
returned to an injected caller as an error; the client stays up. One query at a time, 1 MiB
bodies, `timeout_secs` (15). No bootstrap.
