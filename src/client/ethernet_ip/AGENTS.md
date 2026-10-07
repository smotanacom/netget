# EtherNet/IP explicit messaging

This scope implements TCP encapsulation on port 44818, RegisterSession,
UnregisterSession, CPF unconnected SendRRData and CIP GetAttributeSingle/
SetAttributeSingle with 8/16-bit logical class/instance/attribute paths.
ListIdentity works over TCP and unicast UDP on the same port. It advertises a
NetGet simulator with vendor 0, rather than claiming a manufacturer's identity.
The scanner has discover/get/set actions, typed scalar values and correlation
checks for session, command and context. Set operations use numeric/bool/string
fields, never encoded blobs. The server decodes SetAttributeSingle using standard
Identity types or declared `attribute_types` schemas; schemas store no values.
Handlers decide read values, write approval or a CIP status error; omission is
CIP privilege violation 0x0f. Unsupported services/path/session have wire errors.

Cyclic I/O, ForwardOpen, routing/UnconnectedSend wrappers, symbolic Logix tags,
assembly/tag storage and multicast broadcast discovery are not implemented.
Maximum frame is 4096 bytes, attribute set data 1024 bytes, 128 configured
schemas. Shared runtime limits are 256 connections, 30 s first frame, 600 s
idle and 10 s scanner exchange. Both sockets and all tasks belong to the instance.
Experimental, verified against unchanged cpppo 5.2.5 in both directions.
