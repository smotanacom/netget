# iec104 selected scope

Selected IEC104 APCI and ASDUs: 1 binary,13 float,45 direct single command,
100 general interrogation,102 read. Handler-sourced telemetry has up to eight
points {kind,ioa,value,quality}; explicit command approval is required.
Client receives spontaneous binary/float telemetry as well as polled responses.
Select commands are refused; no timestamped ASDUs, files, clock sync or redundancy.
Sequence numbers wrap at 32768, k=12; immediate acknowledgments satisfy w=8/t2.
t1=15 seconds and t3=20 seconds govern acknowledgments and TESTFR; client exchanges
have a 10-second deadline. Accumulators survive cancellation. Both server and
client tasks/sockets belong to NetGet owners.
Actions: iec104_measurements, iec104_command_result; client iec104_interrogate,
iec104_read,iec104_command, all with common_address.
Independent lib60870 2.3.4 is a test-only executable, never linked into NetGet.

See tests/helpers/ICS_PEERS.md for reproducible independent peers.
