# Redfish client — Experimental

Reads the service root (`scheme` https by default, `insecure` for self-signed BMC certificates)
and refuses anything that is not a `#ServiceRoot.`; with `username`, logs in by session
(`POST` to the root's `Links.Sessions`, keeping `X-Auth-Token` and `Location`; any 2xx — the DMTF
mockup answers 204) or Basic (`auth_method`). `redfish_connected` lists the root's links.

`redfish_get`, `redfish_patch` (optional If-Match), `redfish_post`, `redfish_delete` and
`redfish_action` (reads the resource, takes `Actions["#X.Y"].target`, POSTs the parameters) send
`OData-Version: 4.0`; a 202 is followed through its task monitor until the operation's answer
(60 s), reporting the task's final state and messages. `redfish_response` carries status, body,
Location and, for errors, the Base `MessageId` and message. `disconnect` deletes the session.
