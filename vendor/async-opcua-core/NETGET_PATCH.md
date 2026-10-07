async-opcua-core 0.19.0, MPL-2.0. NetGet changes tcp_codec.rs to inspect a complete
8-byte header immediately and reject chunk lengths below eight or above the
configured message bound before waiting for or allocating the advertised body.
