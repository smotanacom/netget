# SunRPC server tests

No LLM calls: a python policy is the model.

- **Queries:** answers with a fixed table (the portmapper itself, NFS v3/v4 on tcp 2049,
  mountd v3 on 20048).
- **Registrations:** accepts only the user range (0x20000000+).

## `rpcinfo_reads_netget` (Linux; root or passwordless sudo, iproute2, iptables)

libtirpc's `rpcinfo` only ever asks port 111. It runs in a network namespace whose
`OUTPUT` nat chain DNATs tcp and udp 111 to NetGet's port on the host end of a veth (the
`helpers::netns` helper vxlan uses).

| Command | What it exercises |
|---|---|
| `-p` | PMAP v2 DUMP |
| `-s` | RPCBIND v3 DUMP, summarised with the owner |
| `-l` | v4 GETADDRLIST: the universal address `….8.1` for 2049 |
| `-T tcp` | v4 GETADDR then NULL, over TCP |
| `-u` | the same, over UDP |
| an unregistered program | "not registered" |

What NetGet saw is asserted too. libtirpc resolves through v4 GETADDR even for `-u`. The
test does not assume PMAP GETPORT.

## Other tests

- `registrations_errors_and_bounds`: python-assembled XDR (`struct`, not NetGet's encoder).
  - v2 and v4 SET, a refused SET, and v4 UNSET.
  - GETPORT, including the any-version fallback.
  - What the model was shown, AUTH_SYS credentials included.
  - Then hand-written calls for RPC_MISMATCH, PROG_UNAVAIL, PROG_MISMATCH, PROC_UNAVAIL
    (CALLIT), GARBAGE_ARGS, NULL and GETTIME.
  - A record announcing `MAX_RECORD + 1`, which closes the connection unread.
- `a_failed_handler_answers_system_err`: UDP, with an unreachable model.
- `replies_through_the_pcap_oracle`: tshark's rpc/portmap dissectors read NetGet's TCP
  replies to two DUMPs, GETPORT and GETADDRLIST.
