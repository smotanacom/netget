# RCON client

`connect` logs in with the `password` startup parameter (login id 0, which srcds echoes and
servers that always answer 0 agree with), skipping srcds' empty response before the
AUTH_RESPONSE; a refusal fails the connect. Each `rcon_command` is sent with an id that grows
by two; in the `source` dialect it is followed by an empty RESPONSE_VALUE sentinel (id + 1)
and output is collected from every packet with the command's id until the sentinel's mirror
arrives, skipping packets for earlier ids. The `minecraft` dialect reads one packet per
command. The password never appears in an event; packets are capped at 4096 bytes and output
at 1 MiB, with 30 s deadlines.
