# NETCONF server evidence

Codec regressions exercise independent literal framing, negotiation transition,
header-before-body rejection, exact byte/chunk/queue/XML bounds, namespace URI
resolution and inherited QName context, mixed text/CDATA and escaped attributes.
Malformed XML, duplicate expanded attributes, DTD/external entities, unknown
entities and undeclared/reserved namespace prefixes must fail.

Native runtime evidence must additionally use unchanged ncclient0.7.1 with owned
HOME, disabled ambient agent/key/config discovery and independently trusted owned
Ed25519 host keys. Require live server connection state until native EOF, and peer
socket closure on server removal; codec evidence does not substitute for these.
