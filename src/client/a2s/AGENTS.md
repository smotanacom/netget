# A2S client (Steam/Source server query)

A connected UDP socket. Each `a2s_query {query}` sends the request (players and rules with
the "give me a challenge" value), retries once with a challenge the server hands out,
reassembles Source split packets in any order, and decodes the answer into `a2s_response`.
A lost or unanswered query is logged and does not end the client. Uncompressed Source
answers only; 5 s per reply datagram, 64 KiB and 64 packets per answer.
