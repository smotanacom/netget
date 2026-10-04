# AMQP 1.0 client — Experimental

Uses `src/server/amqp1/`'s codec. Connect performs SASL (`sasl`: anonymous, plain with
`username`/`password`, or none), opens with `hostname`, begins one session and reports
`amqp1_connected`. `amqp1_send` attaches a sending link per address on first use, waits for
credit, transfers (split to the server's max-frame-size) and reports the disposition as
`amqp1_outcome` (accepted, rejected, released, modified, or refused when the link was not
attached). `amqp1_receive` attaches a receiving link, grants `count` credit, accepts what arrives
within `timeout_secs`, detaches and reports `amqp1_messages`. A null terminus in the server's
attach counts as a refusal only if its detach follows within a second: rhea's broker answers an
accepted attach without echoing the terminus.
