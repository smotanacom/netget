# bacnet selected scope

BACnet/IP BVLC, local NPDU and unsegmented APDU. Unicast WhoIs/IAm discovery;
server also accepts broadcast WhoIs. ReadProperty/WriteProperty carry primitive
null/boolean/unsigned/signed/real/string/enumerated/object_identifier values,
optional array_index and write priority. Writes need explicit accepted=true.
Missing decisions return write-access-denied; typed Error, Reject and Abort are
reported by the client. Segmentation is unsupported and aborts; COV and other
unsupported confirmed services reject. No BBMD, routed networks,
ReadPropertyMultiple, constructed values or stored object state.
480-byte datagrams, 256 concurrent handler turns, 10-second response deadlines.
Optional device_id defaults to DEFAULT_DEVICE_ID=1234; vendor 0 denotes simulator.
Actions: bacnet_reply; client bacnet_discover,bacnet_read,bacnet_write.
Independent bacpypes3 0.0.102 verifies discovery, reads, writes and errors.

See tests/helpers/ICS_PEERS.md for reproducible independent peers.
