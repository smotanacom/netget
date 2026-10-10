# ActivityPub client (one actor)

Federation has no client that is not also a server: whoever a NetGet actor follows fetches its
key to verify the Follow and delivers its Accept to the actor's inbox. So the client is one
actor (`username`, default `netget`) served by the server's own `instance.rs` and `http.rs` on
`listen` (default `127.0.0.1:0`), with ids built from `base_url` (default `http://` and that
address). The remote must be able to reach it.

`remote_addr` is whoever the client was opened at — a handle, actor URL or host — and is passed
to the handler in `activitypub_ready`; nothing is fetched until an action asks.

## Events

- `activitypub_ready {actor_id, handle, remote}` — the actor is served.
- `activitypub_response {operation, ok, result?, error?}` — every action's outcome, including
  delivery statuses (`follow`, `like`: `{actor, status}`; `post`: `{note_id, deliveries}`;
  `lookup`: the actor's profile; `fetch`: the object).
- `activitypub_activity {type, actor, actor_handle, object_type, object_id, content,
  in_reply_to}` — a signed activity arrived at the inbox and was verified exactly as on the
  server (signature, digest, date, actor = signer). Accept/Reject of our Follow is booked before
  the event.

## Actions

`activitypub_lookup {target}` (WebFinger + a signed GET of the actor), `activitypub_fetch
{url}`, `activitypub_post`, `activitypub_like`, `activitypub_follow`, `activitypub_unfollow`
(the server's, without `as`), and `disconnect`. `activitypub_accept`/`reject` are refused: a
client answers no Follows.

Every action is checked before it runs (`execute_action`); a refused one is answered
`Rejected` to an injected command and logged. Answers to events run in order through one
session loop; a chain stops after `MAX_FOLLOWUP_DEPTH` (8). The endpoint serves at most
`MAX_ENDPOINT_CONNECTIONS` (64) at once; every task it spawns is registered on the client.
