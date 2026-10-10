# WS-Discovery client tests

No LLM calls. `real_server_test.rs` runs two independent target services on the multicast
group. Each test takes a static mutex, because both own UDP 3702 for their duration.

- **wsdd** (`apt-get install wsdd`): the WSD host daemon Linux ships for Samba hosts, run with
  a fixed `-U` UUID.
  1. On `wsd_ready` the client probes the group for `wsdp:Device`.
  2. wsdd's ProbeMatch carries no XAddrs, so the model resolves that endpoint.
  3. The ResolveMatch with `http://…:5357/<uuid>` can only be wsdd's answer to that Resolve.

  wsdd matches Types by literal text, so the probe being answered at all is the check that
  NetGet writes `wsdp:Device` with its conventional prefix. wsdd turns `IP_MULTICAST_LOOP` off
  on its send socket, so its Hello and Bye never reach a listener on the same host. That is why
  announcements are tested against python.
- **python WSDiscovery** (`pip install WSDiscovery`, interpreter from `NETGET_WSD_PYTHON`)
  publishes one NetworkVideoTransmitter with scope `onvif://www.onvif.org/location/office`.
  1. The client listens with `listen_announcements` before the publisher starts, and the
     publisher's Hello arrives as `wsd_announcement`.
  2. On that Hello the model probes with scope `onvif://www.onvif.org/location`, which finds
     the service with types, scopes and XAddrs.
  3. On that answer the model probes with `…/location/kitchen`, which finds nothing.
  4. An injected `wsd_resolve` returns the XAddrs as its `Executed` detail.
  5. An injected probe with an unknown prefix is `Rejected`.
  6. Closing the publisher's stdin makes it send Bye, which the client hears.

Both fail rather than skip without their peer.
