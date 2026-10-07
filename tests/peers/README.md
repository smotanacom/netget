# Peer-to-peer and news interoperability peers

NetGet's six new role pairs are Experimental. These fixtures use independently developed stacks: ncdc 1.25 (NMDC/ADC hub and file transfers), uhub 0.8.0 (ADC hub), aioslsk 1.6.4 (Soulseek central/peer message serialization), gtk-gnutella 1.3.1 (Gnutella peer) and Python 3.10 nntplib / nntpserver 0.0.3 (NNTP). The test fixtures supply local content and rendezvous; they do not replace the independent wire codecs. No public Soulseek/Gnutella services are contacted.

Run the bootstrap script, export the paths it prints, then run the tests in each protocol's test documentation. Rust compiler builds must use the serialized repository guard when sharing the protocol-expansion checkout. Python 3.10 is required because newer Python versions removed the stdlib NNTP client.

The Gnutella fixture disables automatic host fetching, DHT, UDP, UPnP/NAT-PMP and compression, and binds to loopback. Its upstream build uses `--topless --disable-malloc --disable-gnutls --disable-dbus --disable-nls`: no protocol code is patched. macOS's recent Clang additionally needs generated pseudo-file dependency removal and its legacy pointer warning downgraded. This is an independent test-peer compiler profile, not a NetGet dependency.

GPL/AGPL test peers remain standalone external processes; their libraries are not distributed in NetGet. aioslsk and nntpserver fixtures use library encoders/handlers directly in their separate Python process. The ncdc uploader waits for its content hashing and file-list refresh before becoming ready. Tests have fixed deadlines and explicit teardown.

On macOS ARM64, the bootstrap omits gtk-gnutella’s optional `vsort_init` startup benchmark: repeated launches on macOS 27 reproduced an upstream `tqsort`/`thread_join` deadlock before socket initialization (native stack captured). The upstream sorting module supplies defaults when that initialization is omitted. Protocol and network code are unchanged; Linux keeps the benchmark. This fixture workaround does not modify any handshake or descriptor.
