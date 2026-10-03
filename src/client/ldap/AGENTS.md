# LDAP Client Protocol Implementation

## Overview

LDAP (Lightweight Directory Access Protocol) client for connecting to LDAP directory servers and performing directory
operations. Supports bind (authentication), search, add, modify, and delete operations with full LLM control.

## Library Choices

- **ldap3 v0.11** - Mature Rust LDAP client library
- ASN.1/BER encoding handled by ldap3 library
- Synchronous API wrapped with tokio::task::spawn_blocking for async compatibility
- Chosen for protocol compliance, ease of use, and mature implementation

## Architecture Decisions

### Connection Model

LDAP connections are stateful and persistent:

- **LdapConn** - Synchronous connection wrapped in Arc<Mutex> for thread-safety
- **Blocking operations** - All LDAP operations (bind, search, add, modify, delete) use spawn_blocking to avoid blocking
  async runtime
- **No reconnection** - Connection persists until explicit disconnect or error
- **Single connection per client** - Each client instance manages one LDAP connection

### LLM Integration

- **Event-driven** - LDAP operations trigger events that call LLM for next action
- **Four event types**:
    - `ldap_connected` - Initial connection established (`remote_addr`)
    - `ldap_bind_response` - `dn`, `success`, `message`
    - `ldap_search_results` - `base_dn`, `filter`, `success`, `message` (when refused),
      `entries`, `count`
    - `ldap_modify_response` - answers an add, modify **or** delete: `operation` (`add` /
      `modify` / `delete`), `dn`, `success`, `message`. Without `operation` and `dn` the model
      could not tell which of several writes a response belonged to
- **Action-based operations** - LLM returns JSON actions for directory operations
- **The chain is followed and bounded.** Every response's answer is executed and its own
  response comes back in turn (`execute_ldap_action` ↔ `call_llm_with_event`, a boxed `+ Send`
  future), so bind → search → add → modify → delete is one chain of model decisions. It is
  bounded by `MAX_FOLLOWUP_DEPTH` (4): the response at the bound is shown to the model and its
  answer dropped with a warning. `tests/client/ldap/real_server_test.rs` asserts the bound from
  slapd's own log (exactly five searches for a model that searches forever), verified by
  removing it

### Authentication

- **Simple bind** - Username/password authentication (DN + password)
- **Anonymous bind** - Empty credentials (not implemented but supported by ldap3)
- **No SASL** - SASL authentication not implemented
- **Bind state** - Authentication state managed by ldap3 library

### Search Operations

- **Scope control** - Base, OneLevel, or Subtree scope
- **Filter syntax** - Standard LDAP filter syntax (e.g., "(objectClass=person)", "(cn=john)")
- **Attribute selection** - Specify attributes to retrieve or use "*" for all
- **Result parsing** - Search results converted to JSON with DN and attributes; attributes
  whose values are not UTF-8 (`jpegPhoto`, certificates) are left out
- **A refused search is a response, not an error** - `noSuchObject`, a bad filter or
  insufficient access produce `ldap_search_results` with `success: false` and the server's
  result in `message` (e.g. `rc: 32`), exactly as a failed bind or add does. Only a transport
  failure ends the chain without an event

### Modify Operations

Three modification types:

- **Add** - Create new LDAP entry with DN and attributes. Each attribute value may be an array
  of strings or a single string (`{"cn": "Ada"}`); anything else is rejected as a bad action
  rather than silently dropped, because a dropped required attribute comes back from the
  server as an object class violation naming an attribute the model did supply
- **Modify** - Modify existing entry (add/delete/replace attribute values)
- **Delete** - Delete existing entry by DN

## Response Actions

The LLM controls LDAP client behavior through these actions:

- `bind` - Authenticate with DN and password
- `search` - Search directory with base DN, filter, attributes, scope
- `add` - Add new entry with DN and attributes
- `modify` - Modify entry with operation (add/delete/replace), attribute, values
- `delete` - Delete entry by DN
- `disconnect` - Close LDAP connection
- `wait_for_more` - Wait for more LLM actions (used in async responses)

## Event Flow

### Typical Workflow

1. **Connect** → LDAP_CLIENT_CONNECTED_EVENT
2. **LLM decides** → bind action
3. **Bind** → LDAP_CLIENT_BIND_RESPONSE_EVENT
4. **LLM decides** → search action
5. **Search** → LDAP_CLIENT_SEARCH_RESULTS_EVENT
6. **LLM decides** → add/modify/delete actions or disconnect
7. **Modify** → LDAP_CLIENT_MODIFY_RESPONSE_EVENT
8. **LLM decides** → disconnect action

### Example Interaction

```
User: "Connect to LDAP at localhost:389, bind as cn=admin,dc=example,dc=com with password 'secret' and search for all users"

1. LDAP client connects → Event: ldap_connected
2. LLM returns: {"type": "bind", "dn": "cn=admin,dc=example,dc=com", "password": "secret"}
3. Client performs bind → Event: ldap_bind_response (success)
4. LLM returns: {"type": "search", "base_dn": "dc=example,dc=com", "filter": "(objectClass=person)", "scope": "subtree"}
5. Client performs search → Event: ldap_search_results (entries)
6. LLM returns: {"type": "disconnect"}
7. Client disconnects
```

## Data Structures

### Search Result Entry

```json
{
  "dn": "cn=john,dc=example,dc=com",
  "attributes": {
    "cn": ["john"],
    "mail": ["john@example.com"],
    "objectClass": ["person", "inetOrgPerson"]
  }
}
```

### Add Entry Attributes

```json
{
  "objectClass": ["person", "inetOrgPerson"],
  "cn": ["newuser"],
  "sn": ["User"],
  "mail": ["newuser@example.com"]
}
```

### Modify Operation

```json
{
  "type": "modify",
  "dn": "cn=user,dc=example,dc=com",
  "operation": "replace",
  "attribute": "mail",
  "values": ["newemail@example.com"]
}
```

## State Management

- **Connection state** - Tracked in AppState as ClientStatus (Connected/Disconnected/Error)
- **Authentication state** - Managed internally by ldap3 library
- **Memory** - LLM conversation memory stored per client in AppState
- **No local cache** - Directory data not cached, always fetched from server

## Limitations

- **No LDAPS** - TLS encryption not implemented (plain LDAP only)
- **No SASL** - Only simple bind authentication supported
- **No StartTLS** - Cannot upgrade plain connection to TLS
- **Synchronous library** - ldap3 is synchronous, wrapped with spawn_blocking
- **No paging** - Large search results not paged (could exhaust memory)
- **No referrals** - LDAP referrals not followed
- **No schema introspection** - LLM must know schema/objectClasses
- **No binary attributes** - Binary attributes (photos, certificates) not handled
- **No connection pooling** - Each client instance creates new connection

## Performance Considerations

- **Blocking operations** - All LDAP ops use spawn_blocking, may create thread pressure
- **No async** - ldap3 library is synchronous, not optimal for high concurrency
- **Search size** - Large searches (1000+ entries) can be slow, no streaming
- **Memory usage** - Search results loaded entirely into memory

## Error Handling

- **Bind errors** - Invalid credentials return bind response with success=false
- **Search errors** - a search the server refuses is reported to the model as
  `ldap_search_results` with `success: false`
- **Modify errors** - Entry not found, constraint violations return response with success=false
- **Connection errors** - Network errors propagate to ClientStatus::Error
- **LLM errors** - Logged and reported to status_tx, don't crash client

## Security Considerations

- **Plain text** - Credentials sent in plain text (no TLS)
- **No certificate validation** - N/A for plain LDAP
- **Password exposure** - Passwords in LLM action JSON (logged in debug mode)
- **Directory exposure** - LLM can search entire directory (no access control)

## Example Prompts

### Basic Authentication

```
Connect to LDAP at ldap.example.com:389, bind as cn=admin,dc=example,dc=com with password 'adminpass'
```

### Search Users

```
Connect to LDAP at localhost:389, bind as cn=readonly,dc=corp,dc=com and search for all users with mail attribute
```

### Add Entry

```
Connect to LDAP at localhost:389, bind as cn=admin,dc=example,dc=com and add user cn=bob,ou=users,dc=example,dc=com with mail bob@example.com
```

### Modify Entry

```
Connect to LDAP at localhost:389, bind as admin and change mail for cn=alice,dc=example,dc=com to alice.new@example.com
```

## Testing

See `tests/client/ldap/CLAUDE.md`. The evidence is `tests/client/ldap/real_server_test.rs`,
against OpenLDAP's `slapd`.

## References

- RFC 4511 - LDAP: The Protocol
- RFC 4510 - LDAP: Technical Specification Road Map
- ldap3 crate documentation: https://docs.rs/ldap3/

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` can inject an action into a running LDAP client. The channel is
registered with `command_support::register_command_channel` **before** the `ldap_connected`
LLM call and drained by its own task (`command_loop`), registered through
`register_client_task`. Registering first is what makes `[ send ]` usable while a manual `*`
routing rule has the connect event parked waiting for a human.

Both the LLM path (`execute_ldap_action`) and injected commands go through one `apply_action`,
so an injected `bind` / `search` / `add` / `modify` / `delete` produces the same LDAP messages
as one the model asked for, and the matching response event
(`ldap_bind_response` / `ldap_search_results` / `ldap_modify_response`) fires either way. On
the injected path that event is raised **after** the outcome is replied, so `[ send ]` is not
held for an LLM round-trip.

**Outcome semantics** (`ClientSendOutcome`):

| Situation | Outcome |
|---|---|
| an operation completed | `Executed { detail }` — the DN and **the server's own result message**, success or failure |
| a result with no wire effect (`wait_for_more`, unhandled custom) | `Executed { detail }` naming which |
| unknown action type or bad parameters | `Rejected { error }` |
| `disconnect` | the connection is unbound first, then `Disconnected`; the loop ends and the handle is dropped |
| the operation errored at the transport level | `Err` — `send_to_client` returns the error |

**Never `Sent`.** `ldap3`'s synchronous `LdapConn` owns the socket, so NetGet cannot know how
many bytes an operation put on the wire. A failed bind is still `Executed`: the action *did*
reach the server and the server *did* answer — the detail carries that answer verbatim rather
than being upgraded to a success or downgraded to an error.

Test: `tests/client/ldap/command_channel_test.rs` (zero LLM calls; a NetGet LDAP server with a
zero-action `*` static handler falls through to its own fail-closed default, which is the only
way to get a response carrying the request's real messageID — a static handler cannot echo it).

## Maturity: Beta

Rated against the four-condition client bar in the root `CLAUDE.md`, on the evidence in
`tests/client/ldap/real_server_test.rs` (see `tests/client/ldap/CLAUDE.md`):

1. **Real third-party server** — OpenLDAP's `slapd` (C), configured per test with an `mdb`
   database, seeded with `ldapadd` and read back with `ldapsearch`. NetGet's side is the `ldap3`
   crate, which shares no code with OpenLDAP. (NetGet's own LDAP *server* is `ldap3_proto`; it
   is not involved.)
2. **Fails rather than skips** — a missing `slapd`, `ldapadd` or `ldapsearch`, or no OpenLDAP
   schema directory, is a test failure naming the brew formula and the Ubuntu package;
   nothing is `#[ignore]`d. CI's `registry-audit` installs `slapd` and runs the suite in its
   evidence loop.
3. **A real session** — simple bind as the rootdn, a one-level search, and add / modify /
   delete, each response parsed and handed to the model.
4. **Acts on the model's answer, asserted on the wire** — `ldapsearch` finds the entry the model
   added (with a description it built from the search result), the mail it replaced, and the
   entry it deleted gone; after a refused search it finds the OU the model created in response.
   Verified by mutation: dropping the actions the model returns makes all three tests fail.

Not covered by that evidence: SASL, LDAPS/StartTLS, paged results, referrals, binary
attributes.
