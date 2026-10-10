# Consul client

One HTTP/1.1 connection per request (hyper). Actions: `consul_kv_get {key, recurse?,
keys_only?}`, `consul_kv_put {key, value, encoding?, flags?, cas?}`, `consul_kv_delete`,
`consul_catalog {endpoint: services|service|health|nodes|agent_services, name?}`,
`consul_register_service {name, id?, address?, port?, tags?}`, `consul_deregister_service`.
Each answer is one `consul_response {operation, status, result, index, message}`: KV entries
come back base64-decoded to `{key, value, encoding, flags, modify_index}`, catalog and health
entries reduced to `{id, name, address, port, tags, status}`. Answers are capped at 1 MiB; a
handler chain stops after 8 follow-ups. No blocking queries, sessions, ACL tokens or TLS.
