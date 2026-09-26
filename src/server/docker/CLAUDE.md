# Docker Engine API

NetGet answers the read-only core of the Docker Engine API well enough for the real
`docker -H tcp://127.0.0.1:<port>` CLI: `version`, `info`, `ps [-a]`, `images`, `inspect`,
`network ls`, `volume ls`. The model decides which containers, images, networks and volumes
exist; NetGet owns negotiation, routing and the shape of every document.

**State**: Experimental. **Privilege**: `None` (2375 is unprivileged). **Feature**: `docker`
(needs only `urlencoding` for query decoding). **Group**: AI & API. **Keywords**: `docker`,
`dockerd`, `docker engine`, `docker api`, `moby`.

## Library choice

hyper HTTP/1.1, the same shape as `kubernetes`. No `bollard` or Docker types crate: the CLI
decodes JSON into its own Go structs, so what matters is the field names and the fields it
dereferences, and `api.rs` writes those directly. That keeps the feature dependency-free.

## What the CLI sends (captured from Docker CLI 29.8.0)

Every command is `HEAD /_ping`, then one or more `GET /v<negotiated>/…`. The CLI reads the
`API-Version` header from `/_ping` and downgrades to it ("1.47 (downgraded from 1.56)").
`docker inspect <unknown>` tries `/containers/<x>/json`, then `/images/<x>/json`,
`/networks/<x>`, `/volumes/<x>`, `/plugins/<x>/json`, and reads `/info` to decide whether to try
the swarm types — so an unknown name costs two model calls (the container and `/info`).

## What NetGet decides vs what the model decides

| Served deterministically, no model call | Decided by the model (`docker_api_request`) |
|---|---|
| `HEAD`/`GET /_ping` with `API-Version`, `Server: Docker/<engine> (linux)`, `Ostype`, `Docker-Experimental` | `/version` → `send_docker_version` |
| `/vX.Y/` above `api_version` → 400 "client version X is too new…"; below 1.24 → 400 "too old" (Docker's own wording) | `/info` → `send_docker_info` |
| every `POST`/`PUT`/`DELETE` → 501 `{"message": "…read-only…"}` | `/containers/json` → `send_docker_containers` |
| any other `GET` → 404 `{"message": "page not found"}` (this is what `docker inspect`'s fallbacks hit) | `/containers/{id}/json` → `send_docker_container` |
| the filler fields in every document | `/images/json`, `/networks`, `/volumes` → `send_docker_images` / `_networks` / `_volumes` |
| | any of them → `send_docker_error {status, message}` |

The event is `docker_api_request {method, path, api_version, query, resource, id?}`; `path` is
without the version prefix, `query` is decoded (`all`, `filters` as its JSON string), `resource`
is one of `version`, `info`, `containers`, `container`, `images`, `networks`, `volumes`, and
`id` is set for `container`.

**The server picks the action that answers the route.** A response carrying several actions —
which is what a static handler is, since it answers every `docker_api_request` identically — is
scanned for the one whose name matches the route; the rest are ignored. So a fixed host is one
static rule with one action per route (`example_static_actions()` in `actions.rs`), and a model
that answers `/containers/json` with `send_docker_version` gets `decision=fail_closed_no_action`,
not a wrong document.

## Rendering (`api.rs`)

Accepts snake_case (`private_port`) and Docker's own PascalCase (`PrivatePort`) for every field.
Fills: a 64-hex ID derived deterministically from the name when none is given (so `ps` and
`inspect` agree), `/`-prefixed names, `ImageID`, `Status` from `State` (`Up`, `Exited (N)`…),
`HostConfig`, `NetworkSettings`, `Mounts`, `SharedSize: -1`, `Containers: -1`, the `Components`
list of `/version`, and the ~40 fields of `/info` (`Swarm.LocalNodeState: inactive`, plugins,
runtimes). `created` is unix seconds or RFC 3339.

Refuses, with a reason: a container with no name or image, a name Docker would refuse
(`[a-zA-Z0-9][a-zA-Z0-9_.-]*`), a state outside `created|running|paused|restarting|removing|
exited|dead`, a port outside 1..=65535, a port type other than tcp/udp/sctp, a non-alphanumeric
ID, an unparseable timestamp, a tag with whitespace, an `api_version` that is not `N.M`, a
negative count. Refusals happen in `execute_action`, so they reach the log and access log
(`list_access_logs`) as a failed action; the network path does not re-prompt the model with them.

## Failure behaviour

| Condition | Wire | Log |
|---|---|---|
| model answered the route's action | 200 + document | `decision=model_answer action=…` |
| model answered `send_docker_error` | its status + `{"message": …}` (one line) | `decision=model_reject` |
| action refused by the executor | 500 `{"message": "netget: the handler returned no usable answer for this request"}` | `decision=fail_closed_invalid_answer (<reason>)` |
| no action answering this route | same 500 | `decision=fail_closed_no_action` |
| backend saturated | 503 + `Retry-After: 5`, `netget: backend at capacity, retry later` | `decision=fail_closed_llm_error category=Overloaded` |
| backend failed | 500, `netget: request could not be processed` | `decision=fail_closed_llm_error category=Unavailable` |
| mutating endpoint | 501 | `decision=fail_closed_not_implemented` |
| body over the cap | 413 | `decision=fail_closed_body_rejected` |

Never an empty list on failure: `[]` from `/containers/json` means "no containers", and an
outage must not say that. The CLI prints the message as `Error response from daemon: …`.

## Bounds

| Bound | Value | Why |
|---|---|---|
| `MAX_REQUEST_BODY_BYTES` | 1 MiB | reads carry no body and writes are refused; read before routing so it holds everywhere |
| `FIRST_BYTE_READ_TIMEOUT` | 30 s | `peek` before hyper |
| `IDLE_BETWEEN_REQUESTS_TIMEOUT` | 120 s | the CLI connects per command; SDK pools go quiet between calls |
| `MAX_CONNECTIONS` | 256 | refusal is a 503 with a Docker-shaped JSON body |

No peer handle: hyper owns the socket (`HyperOwnsSocket` in
`tests/peer_handle_coverage_ratchet_test.rs`).

## Startup parameters

`api_version` (default `1.47`, must be ≥ 1.24) and `engine_version` (default `27.5.1`), both
read in `spawn()` and validated there.

## Not implemented

Everything that changes state (create/start/stop/exec/pull/build/rm), `/images/{id}/json`,
logs, events, stats, attach, the swarm and plugin endpoints, TLS on 2376, and authentication —
the unauthenticated TCP API on 2375 has none, and neither does this.
