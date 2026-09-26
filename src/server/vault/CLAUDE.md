# Vault (KV version 2)

NetGet answers what HashiCorp's `vault` CLI needs for `vault status` and `vault kv get / put /
list / metadata get` against a KV v2 mount. The model holds the secrets; NetGet stores nothing.

**State**: Experimental. **Privilege**: `None` (8200 is unprivileged). **Feature**: `vault` (no
dependencies beyond the always-present HTTP stack). **Group**: AI & API. **Keywords**: `vault`,
`hashicorp vault`, `openbao`, `secrets engine`, `kv v2`.

## Library choice

hyper HTTP/1.1, the `kubernetes` shape. No Vault crate: the client decodes a small JSON envelope
into its own `api.Secret`, and `api.rs` writes it directly.

## What the CLI sends (captured from Vault CLI 2.0.0)

- `vault status` → `GET /v1/sys/seal-status`, then `GET /v1/sys/leader`.
- every `vault kv …` → first `GET /v1/sys/internal/ui/mounts/<path>`. Its `data.options.version`
  decides the path shape; with `"2"` the CLI uses `<mount>/data/<p>` (get, put) and
  `<mount>/metadata/<p>` (list, metadata get). With anything else it silently falls back to KV
  v1 paths — which is what the first capture against a stub produced.
- `kv put` → `PUT /v1/<mount>/data/<p>` with `{"data": {…}, "options": {…}}`.
- `kv list` → `GET /v1/<mount>/metadata/<p>?list=true` (the Go client turns `LIST` into this).
  `LIST` itself is also accepted.
- Every request carries `X-Vault-Token` and `X-Vault-Request: true`.

## What NetGet decides vs what the model decides

| Served deterministically, no model call | Decided by the model |
|---|---|
| `sys/seal-status` (unsealed, shamir 1/1, `vault_version`, storage `inmem`), `sys/health`, `sys/leader` (HA off) | `vault_read {mount, path, what: "data"\|"metadata", version?}` → `send_vault_secret` |
| the KV preflight: v2 for every path under a configured mount, 403 for any other path | `vault_write {mount, path, data, cas?}` → `send_vault_write_ok` |
| a write body without a `data` object → 400 `no data provided` (Vault's wording) | `vault_list {mount, path}` → `send_vault_list` |
| DELETE / PATCH / undelete / destroy / `config` under a mount → 405 | any of them → `send_vault_error {status, errors}` |
| any other path → 404 `no handler for route "…"` | |
| the envelope: `request_id`, `lease_*`, `wrap_info`, the v2 `data`/`metadata` split, metadata's `versions` map | |

Every event also carries `token_present`, `token_matches_configured` and `token_configured`.
**The token itself is never in an event** — not in the prompt, the log, a script's stdin or the
access log. The comparison is constant-time. With the `token` startup parameter unset, any
non-empty token counts as matching and `token_configured` is `false`; the metadata notes say so.
Refusing a bad token is the model's decision (a 403 via `send_vault_error`), as the design asks:
NetGet does not enforce it.

`what: "metadata"` is answered by the same `send_vault_secret` as a read; NetGet builds the
metadata document (`current_version`, `created_time`, the `versions` map) from its `version` and
`created_time`.

## Rendering (`api.rs`)

Refuses, with a reason, in `execute_action`: a `data` that is not an object, a version that is not
a positive integer, a `created_time` that is not RFC 3339 (normalised to UTC microseconds, the
way Vault writes it), non-object `custom_metadata`, list keys that are not one path segment
(folder names end in `/`), more than 10 000 keys, an error status outside 4xx/5xx. Error strings
become single lines.

## Failure behaviour

| Condition | Wire | Log |
|---|---|---|
| model answered the route's action | 200 + envelope | `decision=model_answer action=…` |
| model answered `send_vault_error` | its status + `{"errors": […]}` | `decision=model_reject` |
| answer refused by the executor | 500 `{"errors": ["netget: the handler returned no usable answer for this request"]}` | `decision=fail_closed_invalid_answer (<reason>)` |
| no action answering the route | same 500 | `decision=fail_closed_no_action` |
| backend saturated | 503 + `Retry-After: 5`, `netget: backend at capacity, retry later` | `decision=fail_closed_llm_error category=Overloaded` |
| backend failed | 500, `netget: request could not be processed` | `decision=fail_closed_llm_error category=Unavailable` |
| unimplemented method | 405 | `decision=fail_closed_not_implemented` |
| body over the cap | 413 | `decision=fail_closed_body_rejected` |

Never a 404 on failure: "No value found" would tell a job the secret was deleted.

## Bounds

| Bound | Value | Why |
|---|---|---|
| `MAX_REQUEST_BODY_BYTES` | 1 MiB | a write's pairs are embedded in a prompt; Vault's own 32 MiB default has no use here |
| `FIRST_BYTE_READ_TIMEOUT` | 30 s | `peek` before hyper |
| `IDLE_BETWEEN_REQUESTS_TIMEOUT` | 120 s | CLI connects per command; SDKs pool |
| `MAX_CONNECTIONS` | 256 | refusal is a 503 with a Vault-shaped JSON body |
| `api::MAX_LIST_KEYS` | 10 000 | render bound |

No peer handle: hyper owns the socket (`HyperOwnsSocket`).

## Startup parameters

`token` (optional), `kv_mounts` (array of mount paths, default `["secret"]`; each segment
`[A-Za-z0-9._-]`, not under `sys`), `vault_version` (default `1.18.3`). All read and validated in
`spawn()`.

## Not implemented

KV v1, delete / undelete / destroy / patch, metadata writes, `sys/mounts` (`vault secrets list`),
token lookup and every auth method, policies, leases, response wrapping, namespaces, every other
secrets engine, TLS.
