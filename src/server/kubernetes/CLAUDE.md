# Kubernetes API Server

NetGet impersonates a `kube-apiserver` convincingly enough that a real `kubectl` talks to it.
The model invents the cluster — nodes, pods, namespaces, CRs — and NetGet supplies the
envelope: routing, discovery, `Table` rendering and the `Status` error object.

**State**: Experimental. **Privilege**: `None` (6443 is above 1023, and any port works).
**Feature**: `kubernetes-server`. **Group**: AI & API. **Keywords**: `kubernetes`, `k8s`,
`kube-apiserver`, `kubectl`.

## The `kubernetes` vs `kubernetes-server` split (read this first)

There are two Kubernetes features and they share nothing:

| Feature | What | Dependencies |
|---|---|---|
| `kubernetes` | the **client** in `src/client/kubernetes/` | `kube` + `k8s-openapi` |
| `kubernetes-server` | **this** | `rustls`/`tokio-rustls`/`rcgen` for the optional TLS listener; otherwise the HTTP stack that is already non-optional |

The server deliberately does **not** depend on `kube` or `k8s-openapi`. `kubectl` speaks JSON to
an apiserver by default — protobuf is a content-type negotiation it only uses when the server
advertises it — so a JSON-only server needs neither the Kubernetes types crate nor `protoc`.
That keeps this protocol buildable in CI and in Claude Code for Web, where the client is not.

Consequence: `kind`/`apiVersion` correctness is our responsibility, not the type system's.
Every response goes through `build_response` in `mod.rs`, which fills them in from the route
when the model omits them.

## Storage: there is none

Per the project rule, no resource store exists in this implementation. NetGet never remembers a
pod between requests. Each list/get/write raises an event; the model (or a script/static
handler) answers it. If the model wants persistence it opts into the generic SQLite facility
(`src/state/sqlite.rs`) like any other protocol.

The one thing that persists for a server's lifetime is the **discovery table**, and that is
configuration supplied at startup, not content.

## What NetGet decides vs what the model decides

This is the one design decision worth defending, so it is written down.

**Served deterministically, no LLM call:**

- `GET /version` — from the `kubernetes_version` startup parameter
- `GET /api`, `GET /apis`, `GET /apis/{group}`, `GET /api/v1`, `GET /apis/{group}/{version}`
- `GET /healthz`, `/livez`, `/readyz`
- `Table` rendering of whatever objects the model returned
- the `Status` envelope around whatever error the model chose

**Decided by the model, one event per request:**

- which objects exist, what they look like, and what their status is
- whether a named object exists at all (`k8s_status` 404 vs `k8s_object_response`)
- whether a create/update/patch/delete is admitted

Why discovery is not an event: `kubectl` performs a full discovery sweep — `/api`, `/apis`, and
one `/apis/{g}/{v}` per advertised group — **before every single command**, and fails with an
unhelpful error if any of it is malformed. Asking a model to reproduce `APIGroupList` correctly
six times per `kubectl get pods` would be slow, expensive and fragile, and none of it describes
the cluster. It describes the *shape of the API*, which is protocol envelope in exactly the
sense that a DNS header is.

The model still controls what is advertised — through the `resources` startup parameter, which
is emitted by the LLM in its `open_server` action. That is also how CRDs are advertised.

There is deliberately **no `k8s_discovery_request` event**. An event that fires only under a
non-default configuration is one grep away from looking like the USB family's never-emitted
events, and the value did not justify the risk.

## Events (3) — all declared with `.with_actions(...)`, all emitted

| Event | Fires on | Actions offered |
|---|---|---|
| `k8s_list_request` | `GET` on a collection (`/api/v1/namespaces/default/pods`, `/api/v1/nodes`) | `k8s_list_response`, `k8s_table_response`, `k8s_status` |
| `k8s_get_request` | `GET` on a named object (`…/pods/web-0`), including subresources | `k8s_object_response`, `k8s_status` |
| `k8s_write_request` | `POST`/`PUT`/`PATCH`/`DELETE` | `k8s_object_response`, `k8s_status` |

All three are raised by `handle_request` in `mod.rs` and all three are exercised by the E2E
suite through real `kubectl` (`get pods`, `get pod web-0`, `delete pod web-0`). Verify with:

```bash
grep -n "K8S_.*_REQUEST" src/server/kubernetes/mod.rs   # the emit side
```

### Event data

`method`, `path`, `api_group`, `api_version`, `group_version`, `resource`, `user_agent`,
`as_table`; plus `namespace`, `name`, `subresource` when the URL carried them, and
`labelSelector` / `fieldSelector` / `limit` / `resourceVersion` when the query did. Writes also
carry `body` (the request body parsed as JSON) or `body_text` when it was not JSON.

## Actions (4) — structured JSON only

No action parameter carries encoded bytes. A Kubernetes object *is* a JSON document, so there
is nothing to base64 and nothing for an executor to forget to decode. If you add an action
here, keep it that way.

- **`k8s_list_response`** — `items` (required array), optional `kind`, `apiVersion`,
  `resourceVersion`. NetGet wraps it in `{kind: "<Kind>List", apiVersion, metadata, items}`, or
  renders a `Table` when the client asked for one.
- **`k8s_object_response`** — `object` (required), optional `status_code` (default 200; 201 for
  a create). `kind`/`apiVersion` are filled from the route when absent.
- **`k8s_table_response`** — explicit `columns` (array of strings) and `rows`
  (`{cells, name, namespace}`), bypassing automatic rendering.
- **`k8s_status`** — `code` (required), `reason`, `message`, `details`. Produces a real `Status`
  object. This is the refusal path, and it is structurally distinct from every success path.

No async actions: the protocol is purely reactive.

## Failure behaviour — no fail-open

- **LLM call fails, backend saturated** (`WireFailure::Overloaded`) → `503` `Status`,
  `reason: ServiceUnavailable`, plus a `Retry-After: 5` header and `details.retryAfterSeconds`,
  so client-go backs off and retries instead of recording a permanent fault.
- **LLM call fails, anything else** (`WireFailure::Unavailable`) → `500` `Status`,
  `reason: InternalError`.
- **Model returns no `k8s_*` action** → `500` `Status` with `reason: InternalError`.
- **Model answered with `k8s_status`** → exactly that `Status`; this is a decision, not a
  failure.

**The peer gets a category, the log gets the error.** The `Status.message` on a failure is a
`&'static str` from `crate::utils::WireFailure::prefixed_text()` — never the error, the backend
URL, the model name or an `anyhow` chain. The full error goes to `tracing::error!` and the
status stream. The three outcomes are separable in the log by their `decision=` tag:
`model_reject`, `fail_closed_no_action`, `fail_closed_llm_error`.

Neither failure branch invents an empty `PodList`. An empty list is a *claim about the cluster* — "there are no
pods" — and it must never be indistinguishable from "the model said nothing". This is the OAuth2
lesson applied here.

## Table rendering

`kubectl get` sends `Accept: application/json;as=Table;v=1;g=meta.k8s.io,application/json` and
prints the returned cells verbatim. `table.rs` derives them per kind, the way a real apiserver's
`TableConvertor` does:

| Kind | Columns |
|---|---|
| Pod | NAME, READY, STATUS, RESTARTS, AGE |
| Node | NAME, STATUS, ROLES, AGE, VERSION |
| Namespace | NAME, STATUS, AGE |
| Service | NAME, TYPE, CLUSTER-IP, EXTERNAL-IP, PORT(S), AGE |
| Deployment / ReplicaSet / StatefulSet | NAME, READY, UP-TO-DATE, AVAILABLE, AGE |
| anything else | NAME, AGE |

Every cell value is read out of the model's object: `status.phase`, `status.containerStatuses`,
`status.conditions[type=Ready]`, `metadata.labels["node-role.kubernetes.io/*"]`,
`status.nodeInfo.kubeletVersion`, and `metadata.creationTimestamp` for AGE (formatted like
Kubernetes' own `duration.HumanDuration`). This is presentation, not invention — and a model
that wants the columns itself answers with `k8s_table_response` instead.

Two rendering leniencies worth knowing: a Pod with no `containerStatuses` but
`status.phase: Running` renders READY as `n/n` rather than `0/n`, and a missing or unparseable
`creationTimestamp` renders AGE as `<unknown>`.

## Routing

```
/version /healthz /livez /readyz          deterministic
/api                                      APIVersions
/api/{version}                            APIResourceList (core)
/apis                                     APIGroupList
/apis/{group}                             APIGroup
/apis/{group}/{version}                   APIResourceList
/api/{v}/{resource}[/{name}[/{sub}]]      cluster-scoped
/api/{v}/namespaces/{ns}/{res}[/{name}[/{sub}]]   namespaced
/apis/{g}/{v}/…                           same two shapes
```

The one subtlety, in `resource_route()`: `namespaces` is both a cluster-scoped resource and the
prefix of every namespaced path. It is treated as a prefix only when at least three segments
follow the group version, so `/api/v1/namespaces` lists namespaces and
`/api/v1/namespaces/default` gets one, while `/api/v1/namespaces/default/pods` is a namespaced
pod list. `resolve_route` is `pub` so this can be tested directly.

**A resource that discovery does not advertise gets a 404 `Status`**, not an event. kubectl
resolves resource names through discovery before it requests anything, so answering an
unadvertised resource would be answering something kubectl could never ask for.

## Startup parameters

Ten, all read in `actions.rs::spawn`, all propagated with `?`:

- the eight shared TLS parameters from `tls_cert_manager::get_tls_startup_parameters()` —
  `tls_enabled` (default **false**), `cert_path`, `key_path`, `common_name`, `san_dns_names`,
  `validity_days`, `organization`, `organizational_unit`
- `kubernetes_version` — reported by `GET /version` (default `v1.29.4`); major/minor are derived
  from it so the three fields cannot disagree
- `resources` — replaces the built-in discovery table **wholesale**, not merged. A malformed
  entry is an error, so a mistyped CRD produces `ServerStatus::Error` rather than a cluster
  quietly missing it.

## Pointing kubectl at it

Plain HTTP is fully supported and is the easy path — `kubectl` is happy with an `http://`
server URL:

```bash
cat > /tmp/netget.kubeconfig <<'EOF'
apiVersion: v1
kind: Config
clusters:
- name: netget
  cluster:
    server: http://127.0.0.1:6443
contexts:
- name: netget
  context: {cluster: netget, user: netget, namespace: default}
current-context: netget
users:
- name: netget
  user: {}
EOF

kubectl --kubeconfig /tmp/netget.kubeconfig get pods
```

For TLS, start with `"startup_params": {"tls_enabled": true}` and add
`insecure-skip-tls-verify: true` under `cluster:` (NetGet serves a self-signed certificate from
the shared `tls_cert_manager`; `cert_path`/`key_path` accept a real one). Use `https://` in the
server URL:

```yaml
  cluster:
    server: https://127.0.0.1:6443
    insecure-skip-tls-verify: true
```

Use `--cache-dir` when experimenting: kubectl caches discovery per host:port under
`~/.kube/cache`, so a restarted server with a different `resources` set will otherwise be
addressed with a stale RESTMapper.

## Not implemented

- **Watch.** `?watch=true` returns a `501` `Status`, deliberately — answering a watch with a
  one-shot list leaves the client reconnecting against a body it cannot decode.
- **OpenAPI.** `/openapi/v2` and `/openapi/v3` are 404, so `kubectl explain` and client-side
  `apply` validation do not work.
- **Protobuf** content negotiation, **server-side apply**, **admission**, **RBAC**,
  **authentication** (every request is served; there is no `Authorization` check).
- **HTTP/2.** ALPN is not advertised, so kubectl uses HTTP/1.1 even over TLS.
- Per-connection tasks are untracked, so `stop_server` does not cancel in-flight requests —
  the same gap every hyper-based protocol here has.

## Testing

`tests/server/kubernetes/e2e_test.rs`, 4 tests, ~10 LLM calls. Two are driven by the **real
`kubectl` binary**; two are wire-level with `reqwest`. See `tests/server/kubernetes/CLAUDE.md`.

```bash
./cargo-isolated.sh test --no-default-features --features kubernetes-server \
    --test server -- --test-threads=100 kubernetes
```
