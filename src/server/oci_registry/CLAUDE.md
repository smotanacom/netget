# OCI Distribution v2 Registry Server (`oci_registry`)

The container-registry API that Docker Hub, GHCR, ECR and Quay speak — OCI
Distribution Specification v1.1. The model invents the repository catalogue, the
tag lists, the manifests, and the blob content; NetGet owns the HTTP, the routing
and, critically, **every digest**.

Feature: `oci-registry`. Deps: `sha2` only (pure Rust) — HTTP is `hyper`, which is
an unconditional dependency. Links no system library, so it is in `portable-base`
(hence `dist` and `dist-darwin`) and in `dist-windows` and `all-protocols`.

**PULL ONLY.** See [Push](#push-is-not-implemented).

## The design problem: content addressing without storage

OCI is content-addressable. A manifest names its config and layer blobs by
`sha256:<hex>`, and every real client re-hashes what it receives. `crane`,
`docker`, `containerd` and `skopeo` all abort on a mismatch — that is the whole
point of the scheme.

NetGet's rule is that a protocol **must not implement storage**. So there is no
blob store to write at push time and read at pull time, and the obvious shortcut —
let the model state a digest and serve whatever it later supplies — produces
images that no real client will accept.

The resolution is **compute, never trust**. Concretely:

| Where a digest could come from | What happens |
|---|---|
| The model writes `digest` in a manifest descriptor | **Overwritten** by `apply_blob_descriptors` with the SHA-256 of the content the model supplied for that descriptor |
| The model writes `digest` on `send_oci_blob` | **Verified** against the computed digest; a mismatch is an error and the action is rejected. Never silently rewritten, never passed through |
| `Docker-Content-Digest` on a manifest response | Computed in `mod.rs` over the exact bytes about to be written to the socket |
| Client asks for a blob by `sha256:X` | The model's content is hashed; if it is not `X`, **404 `BLOB_UNKNOWN`** with both digests in `detail`. The bytes are never served |
| Client asks for a manifest by `sha256:X` | Same, with **404 `MANIFEST_UNKNOWN`** |

`sha256_digest()` in `actions.rs` is the only place a digest is produced, and it is
always called on the byte slice being sent, never on a copy or a re-serialization.
`build_from_action` recomputes the manifest digest at send time rather than trusting
the value `execute_manifest` passed along, so a future refactor cannot desynchronise
the header from the body without the server noticing and refusing.

### How a manifest becomes pullable

`send_oci_manifest` takes the manifest document **and** the content it references:

```json
{
  "type": "send_oci_manifest",
  "manifest": {
    "schemaVersion": 2,
    "mediaType": "application/vnd.oci.image.manifest.v1+json",
    "config": {"mediaType": "application/vnd.oci.image.config.v1+json"},
    "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip"}]
  },
  "blobs": [
    {"role": "config", "content": "{\"architecture\":\"amd64\",\"os\":\"linux\",…}"},
    {"role": "layer",  "content": "a layer's bytes"}
  ]
}
```

The server hashes each blob, writes the real `digest` and `size` into the matching
descriptor (`config`, `layers[i]`, or `manifests[i]` for an index), serializes the
result, hashes *that*, and serves it. The model never has to compute anything, and
the manifest is self-consistent by construction.

Descriptors with no matching blob keep the model's digest, are **logged at WARN**,
and will be refused when a client fetches them. That is visible failure, not silent
corruption.

### The cost, stated plainly

Because there is no store, **the model must return byte-identical content each time
a blob is fetched**. An LLM asked to re-emit the same gzip twice will not. So:

- Use a **script** or **static** event handler for any image a real client will pull.
  A script handler can dispatch on `event.digest` and return the right blob; that is
  exactly how `tests/server/oci_registry/e2e_test.rs` drives `crane`.
- Free-running LLM mode is fine for `crane manifest`, `crane ls`, `crane catalog`
  and for honeypot/impersonation use, where nothing is fetched twice.
- Server memory (`set_memory`) helps but is not a substitute for determinism.

## Endpoints

| Route | Event | Notes |
|---|---|---|
| `GET\|HEAD /v2/` | `oci_version_check` *(only if `version_check=llm`)* | Answered in-process by default |
| `GET\|HEAD /v2/_catalog` | `oci_catalog_request` | `?n=`/`?last=` passed to the model; `next_last` emits a `Link` header |
| `GET\|HEAD /v2/{name}/tags/list` | `oci_tags_request` | Response echoes `name` |
| `GET\|HEAD /v2/{name}/manifests/{ref}` | `oci_manifest_request` | `ref` is a tag or a digest; `by_digest` is in the event |
| `GET\|HEAD /v2/{name}/blobs/{digest}` | `oci_blob_request` | |
| `*/blobs/uploads/*` | — | 405 `UNSUPPORTED`, no LLM call |
| `/v2/{name}/referrers/{digest}` | — | 404 `UNSUPPORTED`, no LLM call (the spec permits this) |
| anything else | — | 404 `UNSUPPORTED`, no LLM call |
| any non-GET/HEAD method | — | 405 `UNSUPPORTED`, no LLM call |

`HEAD` is not special-cased: the full response is built and hyper strips the body
while keeping `Content-Length` and `Docker-Content-Digest`. `crane digest` relies on
this.

Repository names are validated against the spec grammar before the model is asked
(uppercase → 400 `NAME_INVALID`), and digests are syntax-checked (400
`DIGEST_INVALID`). **Only `sha256` is supported**; `sha512:…` is refused explicitly
rather than pretended.

Path parsing uses `rfind`, because a repository name may contain `/` *and* a
component may legitimately be called `manifests`. `/blobs/uploads` is matched before
`/blobs/`, or a push would be misread as a pull of a blob named `uploads`.

## Actions

| Action | Purpose |
|---|---|
| `send_oci_version_ok` | 200 + `Docker-Distribution-Api-Version: registry/2.0` |
| `send_oci_auth_challenge` | 401 + `WWW-Authenticate: Bearer realm=…,service=…,scope=…` |
| `send_oci_catalog` | `{"repositories": [...]}` |
| `send_oci_tags` | `{"name": …, "tags": [...]}` |
| `send_oci_manifest` | manifest or index + optional `blobs` |
| `send_oci_blob` | blob content + `encoding` |
| `send_oci_error` | the OCI error envelope |

Every event declares its own reply actions plus `send_oci_auth_challenge` and
`send_oci_error`, so the model always has both an answer and a refusal.

### Encoding

`send_oci_blob` and `blobs[].encoding` accept `"utf8"` (default), `"hex"` and
`"base64"`. **All three are actually decoded** by `decode_content`, and
`tests/server/oci_registry/digest_test.rs` asserts each one — the `send_tcp_data`
defect (documented as hex-accepting, executor did `.as_bytes()`) is the reason this
is spelled out rather than assumed.

`utf8` is the right answer for config blobs, which are JSON, and for any synthetic
layer. A *real* gzipped tar layer needs `base64` or `hex`, and a model will not
produce one reliably — use a script handler.

## Content-Type

Getting this wrong is the classic reason a client rejects an otherwise valid image.
Priority:

1. the document's own `mediaType` — those bytes are what the client hashes and
   parses, so a disagreeing `Content-Type` is what breaks things;
2. the action's `media_type` parameter;
3. inferred from shape: a `manifests` array ⇒ index
   (`application/vnd.oci.image.index.v1+json`), otherwise image manifest
   (`application/vnd.oci.image.manifest.v1+json`).

Docker media types (`application/vnd.docker.distribution.manifest.v2+json`,
`…manifest.list.v2+json`) work by simply putting them in the document.

## Startup parameters

| Name | Values | Default | Effect |
|---|---|---|---|
| `version_check` | `"auto"` \| `"llm"` | `"auto"` | `auto` answers `GET /v2/` in-process with 200 and the API-version header, no LLM call. `llm` raises `oci_version_check` so the model can demand a token |

`auto` is the default because every client sends `GET /v2/` before every other
request, and paying a model round-trip for a probe with no decision in it doubles
the cost of `crane ls`. It is a deliberate, documented static handler with an
explicit knob — not a fail-open default: in `llm` mode a model that answers nothing
gets a 500, not a 200.

A typo (`"version_check": "yes"`) **fails the server start**. It does not silently
pick a mode.

## Fail-closed behaviour

Per the OAuth2 post-mortem in the root `CLAUDE.md`, the model's refusal and the
model's silence must not look alike:

- **Refusal** — the model returns `send_oci_error` and the client gets the code it
  chose (404 `MANIFEST_UNKNOWN`, 403 `DENIED`, …).
- **Silence** — no usable action: **500 with code `UNKNOWN`** and the message
  "the registry backend returned no usable response". Never an empty catalogue,
  never a plausible 404, never a synthesized manifest.
- **LLM error** — the call to the model failed. The peer gets a *category*, never
  the error text: 503 `UNKNOWN` + `Retry-After: 1` when the backend is saturated
  (so a client backs off) and 500 `UNKNOWN` otherwise. The error itself goes to
  the log and the status stream, tagged `decision=fail_closed_llm_error`;
  silence is tagged `decision=fail_closed_no_action` and the model's own refusal
  `decision=model_reject`.
- **Digest mismatch** — 404, with `detail.requested` and `detail.computed`.

## Push is not implemented

`POST /v2/{name}/blobs/uploads/`, `PATCH`, `PUT` and manifest `PUT` all return
**405 with an `UNSUPPORTED` error envelope**.

This is a deliberate choice, not an omission. Accepting an upload would require
buffering and retaining blob bytes so a later pull could serve them — which is
exactly the storage a protocol is forbidden from implementing. A registry that
accepts a push and then 404s the pull is worse than one that says up front it does
not accept pushes. If push is ever wanted, it needs the generic SQLite facility
(`src/state/sqlite.rs`) as the store, opted into at runtime, not a private buffer
in this module.

## Testing against a real client

`crane` (google/go-containerregistry) is the reference peer. **go-containerregistry
auto-detects `127.0.0.1` and `localhost` as plain HTTP, so `--insecure` is optional
for a loopback registry** — it was verified to work both with and without.

```bash
crane catalog  127.0.0.1:5000
crane ls       127.0.0.1:5000/library/alpine
crane digest   127.0.0.1:5000/library/alpine:latest
crane manifest 127.0.0.1:5000/library/alpine:latest
crane config   127.0.0.1:5000/library/alpine:latest      # verifies the config digest
crane blob     127.0.0.1:5000/library/alpine@sha256:…    # verifies the blob digest
```

Other clients, none of them validated here:

- **`skopeo`** — `skopeo inspect --tls-verify=false docker://127.0.0.1:5000/library/alpine:latest`
- **`oras`** — `oras manifest fetch --plain-http 127.0.0.1:5000/library/alpine:latest`
- **`docker`** — needs `{"insecure-registries": ["127.0.0.1:5000"]}` in
  `/etc/docker/daemon.json` (or Docker Desktop → Settings → Docker Engine) **and a
  running daemon**. Prefer the daemonless clients above.

## Known limitations

- Pull only (above).
- `sha256` only.
- No pagination state: `?n=`/`?last=` are handed to the model, which decides.
- The referrers API is not implemented.
- Bearer-token *challenges* can be issued, but there is no token endpoint — the
  challenge points wherever the model says. `Authorization` headers are passed to
  the model in every event; nothing validates them.
- No `Range` support on blobs; whole blobs only.
- The whole response is built in memory. A model-authored blob is not streamed.
- Per-connection tasks are untracked, so `stop_server` does not cancel a request
  already in flight (a repo-wide issue, not specific to this protocol).
