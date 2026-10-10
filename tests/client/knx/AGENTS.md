# KNX/IP client tests

The client's connect handler switches 1/2/3 on and reads 1/2/4.

- `session_test.rs`: NetGet's own gateway — the feedback and the response decoded by DPT, an
  injected write by explicit DPT, and local refusals (no DPT, a bad address, a value its DPT
  cannot carry).
- `real_server_test.rs`: knxd as the gateway (`-T -S`, dummy bus); knxtool's listener on knxd's
  own socket shows the write and the read arrived, and xknx tunnelled into the same knxd
  (`xknx_peer.py … respond`) answers the read, which NetGet decodes as 19.25.

Peers from `tests/server/knx/install_peers.py`. No LLM calls.
