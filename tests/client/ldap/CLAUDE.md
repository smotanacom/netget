# LDAP Client E2E Tests

Two files, declared in `tests/client/ldap/mod.rs`. Nothing is `#[ignore]`d.

| File | Peer | Tests | LLM calls |
|---|---|---|---|
| `real_server_test.rs` | **OpenLDAP `slapd`** + `ldapadd` / `ldapsearch` | 3 | 19 |
| `command_channel_test.rs` | NetGet's own LDAP server | 1 | 0 |

## Running

`--test` names a **target**, not a module path:

```bash
./cargo-isolated.sh test --no-default-features --features ldap \
    --test client -- client::ldap --test-threads=100
```

## `real_server_test.rs` — the evidence the rating rests on

`start_slapd` writes a minimal `slapd.conf` into the guard's temp dir — the core, cosine and
inetorgperson schemas (from the first of `/opt/homebrew/etc/openldap/schema`,
`/usr/local/etc/openldap/schema`, `/etc/ldap/schema`, `/etc/openldap/schema` that has
`core.schema`), `moduleload back_mdb` (with a `modulepath` where Debian keeps its modules), an
`mdb` database for `dc=example,dc=com` in the temp dir, rootdn `cn=admin,dc=example,dc=com`
with plaintext `rootpw secret` — and runs `slapd -h ldap://127.0.0.1:<probed port>/ -f … -d
stats` in the foreground, ready when it logs `slapd starting`. `-d stats` puts every operation
in the server's log, which the bound test counts. It then seeds the base, `ou=people`, `uid=ada`
and `uid=grace` with `ldapadd` as the rootdn. **It fails, never skips,** when `slapd`,
`ldapadd`, `ldapsearch` or a schema directory is missing, naming `brew install openldap` /
`apt-get install slapd ldap-utils`. Homebrew keeps `slapd` in openldap's `libexec`, which the
helper searches. Ubuntu's AppArmor profile confines `slapd` to its packaged paths; CI unloads
it.

### `ldap_client_binds_searches_and_writes_against_slapd` (7 calls)

`ldap_connected` → bind as the rootdn; the bind response (matched on `dn` and `success`) →
one-level search of `ou=people` for `(uid=ada)` asking for `cn` and `mail`; the results
(matched on `filter`, `count` 1 and the seeded mail) → add `uid=netget` with `cn` and `sn` as
**plain strings** and a `description` built from Ada's `cn` and `mail` as slapd returned them;
the add's `ldap_modify_response` (matched on `operation` add and the DN) → replace Ada's mail;
the modify's → delete `uid=grace`; the delete's → nothing. Then `ldapsearch` must find the
added entry with the model's attributes, Ada's new mail, and no Grace.

### `ldap_client_reports_a_refused_search_to_the_model_against_slapd` (5 calls)

After binding, the model searches `ou=staff`, which does not exist. slapd answers
`noSuchObject`; the model must see `ldap_search_results` with `success` false and `rc: 32` in
`message`, and answers by adding the OU, which `ldapsearch` must then find.

### `ldap_client_follow_up_chain_is_bounded_against_slapd` (7 calls)

The model answers every search result with the same search. The search from the connect event
runs at depth 0; the result at `MAX_FOLLOWUP_DEPTH` (4) is shown and its answer dropped. So the
mock sees five results and **slapd's own log** shows exactly five `SRCH base="ou=people,…"`.

### What the real server found

- **A single-valued attribute written as a string was dropped.** `add` kept only array
  values, so `{"cn": "NetGet Model", "sn": "Model"}` sent an entry without `cn`/`sn` and slapd
  refused it as an object class violation. Verified by mutation: dropping strings again fails
  the first test.
- **A refused search raised nothing.** `search_result.success()?` turned `noSuchObject` into an
  error that ended the chain, so the model never learned why its search came back empty.
  Verified by mutation: propagating the error again fails the second test.
- **`ldap_modify_response` did not say which write it answered.** Add, modify and delete share
  it; it now carries `operation` and `dn`, which is how the first test's three rules tell them
  apart.
- **The chain had no bound.** A model that answered every response with another operation
  looped forever. Verified by removing the bound: the third test fails.

### Why this is condition 4 of the client bar

Every assertion is on slapd's state or slapd's own log, written by operations the model chose —
one built from a search result it was shown, one in answer to a refusal. Verified by mutation:
dropping the actions the model returns for a response makes all three tests fail.

## `command_channel_test.rs` (0 calls)

A `bind` injected through `AppState::send_to_client` (the dashboard's `[ send ]`) reaches a
NetGet LDAP server.

The three `#[ignore]`d tests that needed a Docker OpenLDAP container (`e2e_test.rs`:
connect, bind-and-search, add-modify-delete) were deleted: the real-server test covers all
three against a `slapd` it starts itself.

## Not covered

- SASL, LDAPS and StartTLS; paged results; referrals.
- Attributes whose values are not UTF-8, which are left out of search results.
