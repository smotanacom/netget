# RDAP server — Experimental, RFC 7480 / 9082 / 9083

Plain HTTP/1.1 (hyper). Rust parses every RFC 9082 query under `base_path` before the handler
sees it and enforces the RFC 9083 envelope on every answer; the handler supplies the
registration data. No registry storage in Rust.

`query.rs` (shared with the client): lookups `domain`, `nameserver`, `ip` (address or CIDR),
`autnum`, `entity`; searches `domains?name|nsLdhName|nsIp`, `nameservers?name|ip`,
`entities?fn|handle` (one parameter, `*` in the first label of a name pattern); `help`.
Names are lower-cased and a trailing dot dropped; IPs are canonical; ASNs decimal u32;
values 1–255 bytes with no control characters. Anything else is **400** with an RFC 9083
error object and no handler call; outside `base_path` is 404; methods other than GET/HEAD are
405 with `Allow`.

`rdap_response` takes exactly one of: `object` (objectClassName must match the lookup —
`domain`, `nameserver`, `ip network`, `autnum`, `entity`; a help object needs `notices`),
`results` (searches; each result's class checked, wrapped as `domainSearchResults` etc.),
`not_found`, `error {code 400|403|404|422|429|500|501|503, title, description}`, or `redirect`
(lookups only; 302 to an http(s) URL with no whitespace or controls). Rust adds
`rdap_level_0` to `rdapConformance`, sets `application/rdap+json` and
`Access-Control-Allow-Origin: *`. HEAD gets the same status with no body. A backend failure is
500 (503 + Retry-After when overloaded); an invalid answer is 500 — never a guessed object.
Bounds: 16 KiB headers, 1 MiB / 50 000-node / depth-32 answers, 1000 search results.

Not implemented: TLS (front it with a proxy), bootstrap (RFC 9224), authentication, IDN
U-label conversion, jCard/JSContact validation, extensions beyond passing them through.
