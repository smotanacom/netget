# NetFlow v9 exporter evidence

Use the pinned peer bootstrap/environment described in the server test docs.
The required actual GoFlow2 2.2.7 service independently decodes native exported
IPv4/IPv6, large64bit counters, fixed flags, uptime and options scopes/values;
Count includes all templates/data (seven records across six FlowSets), and two
packets carrying four data records each advance sequence0then1.
Compare decoded values to explicit independent byte expectations. Fixture
readiness retries only a separate literal Source-ID999 packet; production data
commands have no retry. Output order may differ across GoFlow2 workers, so
match actual sequence numbers without assuming stdout order.

Native lifecycle tests check complete validation before send, catalog capacities
and atomicity, packet sequence wrap, template-only refresh increments, live
injection/refresh during a parked handler, disconnect/removal, common scripts
and client memory, followup/action/event caps and unexpected collector replies.
Native pair evidence is separate from the independent receiver service. No
missing-peer skip, ignored test, mock-only interoperability, storage/durability,
fuzz or production capture claim is made.
