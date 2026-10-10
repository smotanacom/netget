# PFCP client tests

No LLM calls. The UPF is **wmnsk/go-pfcp** (`pfcp-peer upf`, from
`tests/server/pfcp/install_peers.py`). It decodes every request with go-pfcp and prints what
it read, so the assertions are on an independent implementation's reading of NetGet's bytes.

The chain:

1. associate;
2. on acceptance, establish a session: an access PDR asking for an F-TEID, a core PDR, a
   FORW FAR to core, and a FORW FAR with a GTP-U outer header;
3. on acceptance, modify FAR 2 to BUFF|NOCP;
4. then delete.

What is asserted:

- Node ID and the recovery time stamp.
- CP SEID 1 in the F-SEID, with header SEID 0.
- The PDRs, FARs and outer header exactly as go-pfcp read them.
- Modification and deletion addressed to the UPF's SEID 0x9999.
- The establishment response's F-TEID, in NetGet's event.
- The UPF's heartbeat answered by NetGet.
- An injected deletion of an unknown session refused locally.

Mutation-checked: discarding the model's actions stalls the chain at the first step.
