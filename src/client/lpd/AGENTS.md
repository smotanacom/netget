# LPD client (RFC 1179)

LPD carries one command per connection, so `connect` only checks reachability and every
action dials anew. `lpd_print` writes a control file (H host, P user, J job name, L banner
user, N source name, the format line and U) and the data file, control first unless
`order: data_first`, and reads each acknowledgement; `lpd_print_result` reports whether the
job was queued and, if not, at which step (`queue`, `control`, `data` or `final`) the server
refused. `lpd_queue`, `lpd_remove` and `lpd_start_queue` report the server's text as
`lpd_reply`. A connection failure is logged and does not end the client.

Job numbers are random three-digit values; host and user come from the `host` and `user`
startup parameters. The source port is unprivileged, so a server that enforces the RFC's
721-731 range refuses it. Replies are bounded to 1 MiB; connect, write and acknowledgement
deadlines are 30 s.
