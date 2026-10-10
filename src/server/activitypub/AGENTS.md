# ActivityPub server (an instance)

Server-to-server ActivityPub over HTTP/1.1 (hyper). One instance hosts one or more local actors
(`actors`, default `["netget"]`); ids are built from `base_url` (default `http://` and the bound
address). No well-known port: an instance is reached through whatever URL fronts it, so the
port is the caller's or OS-assigned. The code is shared with the client role: `instance.rs`
(actors, keys, fetch, delivery, verification), `sig.rs` (HTTP Signatures) and `http.rs` (the
routes) are used by both.

## What Rust owns

- **WebFinger** `/.well-known/webfinger?resource=acct:user@host` (also the actor URL) → JRD
  with a `self` link to the actor. Unknown → 404. **NodeInfo** 2.1 (`/.well-known/nodeinfo`,
  `/nodeinfo/2.1`).
- **Actors** `/users/{name}`: a `Person` with an RSA-2048 key (`publicKey`, `#main-key`),
  `inbox`, `outbox`, `followers`, `following` and `endpoints.sharedInbox`. Keys are generated
  at start (on a blocking thread) and live as long as the server.
- **Collections** `followers`, `following` (accepted follows only), `outbox` (the last
  `OUTBOX_KEPT` Creates, newest first) as `OrderedCollection`s. **Notes** `/notes/{id}` (the
  last `MAX_OBJECTS`).
- **Inboxes** `POST /users/{name}/inbox` and the shared `POST /inbox`. Before anything is
  queued: the body is read up to `MAX_DOCUMENT` (1 MiB, else 413); the `Signature` header
  (draft-cavage, `rsa-sha256` — `hs2019` is read as RSA — at most `MAX_SIGNATURE_HEADER`) must cover
  `(request-target)`, `host`, `date` and `digest`; the key is fetched from the signer's actor
  document (or a key document) and the signature verified; `Digest` must match the body; `Date`
  must be within `MAX_CLOCK_SKEW_SECS` (12 h); and the activity's `actor` must be the key's
  owner. Any failure is 401 with the reason (it describes the peer's own request). A verified
  activity is queued (`INBOX_QUEUE`, 256; full → 503) and answered 202; the model sees them one
  at a time, in order.
- **Bookkeeping** before the model hears of an activity: an `Undo{Follow}` removes the
  follower; an `Accept`/`Reject` of one of our Follows marks it accepted or drops it.
- **Delivery**: every outgoing activity is a signed POST (same headers, plus `Digest`) to the
  recipient's inbox, or the shared inbox for followers that have one, at most
  `MAX_DELIVERIES` (100) per post. Fetches are signed GETs (authorized fetch), bounded at
  `MAX_DOCUMENT`, `HTTP_TIMEOUT` (10 s). Loopback targets bypass any system proxy.
- `MAX_FOLLOWERS` per actor, `MAX_REMOTE_ACTORS` cached remote actors (the cache is cleared
  when full).

## What the model decides

One event, `activitypub_activity {to_actor, type, id, actor, actor_handle, object_type,
object_id, content, in_reply_to}`, for every verified activity. Content is HTML-stripped.
Actions:

- `activitypub_accept` / `activitypub_reject`: answer the Follow being handled (refused for
  any other activity). Accept adds the follower and delivers a signed `Accept`.
- `activitypub_post {content, to?, public?, in_reply_to?, as?}`: a `Note` in a `Create`,
  delivered to followers and to each `to` actor. Content is HTML-escaped, at most
  `MAX_CONTENT` bytes; each line becomes a `<p>`.
- `activitypub_like {object, to?, as?}`, `activitypub_follow {target, as?}`,
  `activitypub_unfollow {target, as?}` — `target` is a URL or `user@host` (WebFinger).

`as` picks the local actor; it defaults to the inbox's owner, then the first actor.

## Failure

The inbox has already answered 202 when the model is asked (that is ActivityPub: delivery and
reaction are separate requests). A failed or invalid model call logs
`decision=fail_closed_llm_error` and **sends nothing** — in particular a Follow is neither
accepted nor rejected, so no follower is ever added without the model's say. Each answer logs
`decision=model_answer|model_silent`; a failed action logs its error and the rest still run.

## Not implemented

Object integrity proofs (FEP-8b32), RFC 9421 HTTP Message Signatures (Fedify retries with
draft-cavage after a 401), authorized-fetch *enforcement* on our own GETs, key rotation (a
cached remote actor keeps its first key until the cache clears), Announce/boost sending, media,
client-to-server ActivityPub, persistence.
