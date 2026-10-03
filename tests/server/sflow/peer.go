// A test-only command around the unmodified BSD-3-Clause Cistern public API.
// No third-party implementation code is included in this repository.
package main

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"time"

	"github.com/Cistern/sflow"
)

const revision = "ed105e3cf9fb208505ed3a9939c9449321cbacf1"

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func main() {
	if len(os.Args) == 2 && os.Args[1] == "version" {
		fmt.Println("Cistern/sflow " + revision)
		return
	}
	if len(os.Args) == 3 && os.Args[1] == "decode" {
		wire, err := os.ReadFile(os.Args[2])
		must(err)
		value, err := sflow.NewDecoder(bytes.NewReader(wire)).Decode()
		must(err)
		must(json.NewEncoder(os.Stdout).Encode(value))
		return
	}
	if len(os.Args) != 4 || os.Args[1] != "emit" {
		panic("usage: peer version | decode FILE | emit UDP_ADDR WIRE_PREFIX")
	}
	header, err := hex.DecodeString("451000400000000040110000c0000202c63364020035007b002c0000")
	must(err)
	// The pinned encoder mistakenly places IndexVal in the high byte and Type
	// in the low byte. Compensate through public API arguments only: this emits
	// the normative source word 0x02000003 (class 2, index 3). Tests must compare
	// these actual bytes with literal expectations and the GoFlow2 collector.
	// Do not patch the library or use its self-roundtrip as a wire oracle.
	samples := []sflow.Sample{
		&sflow.FlowSample{SequenceNum: 5, SourceIdType: 3, SourceIdIndexVal: 2,
			SamplingRate: 1000, SamplePool: 12345, Drops: 4, Input: 3, Output: 4,
			Records: []sflow.Record{
				sflow.RawPacketFlow{Protocol: 11, FrameLength: 64, Stripped: 0, HeaderSize: uint32(len(header)), Header: header},
				sflow.ExtendedSwitchFlow{SourceVlan: 42, SourcePriority: 3, DestinationVlan: 43, DestinationPriority: 4},
			}},
		&sflow.CounterSample{SequenceNum: 17, SourceIdType: 3, SourceIdIndexVal: 2,
			Records: []sflow.Record{
				sflow.GenericInterfaceCounters{Index: 3, Type: 6, Speed: 1000000000,
					Direction: 1, Status: 3, InOctets: 9007199254740999, InUnicastPackets: 11,
					InMulticastPackets: 12, InBroadcastPackets: 13, InDiscards: 14, InErrors: 15,
					InUnknownProtocols: 16, OutOctets: 18446744073709551615,
					OutUnicastPackets: 21, OutMulticastPackets: 22, OutBroadcastPackets: 23,
					OutDiscards: 24, OutErrors: 25, PromiscuousMode: 2},
				sflow.EthernetCounters{AlignmentErrors: 1, FCSErrors: 2, SingleCollisionFrames: 3,
					MultipleCollisionFrames: 4, SQETestErrors: 5, DeferredTransmissions: 6,
					LateCollisions: 7, ExcessiveCollisions: 8, InternalMACTransmitErrors: 9,
					CarrierSenseErrors: 10, FrameTooLongs: 11, InternalMACReceiveErrors: 12, SymbolErrors: 13},
			}},
	}
	// VlanCounters is intentionally absent: its unmodified encoder advertises
	// unsafe.Sizeof's 32 bytes but writes 28. Native VLAN evidence uses a literal
	// wire fixture and GoFlow2 instead; no Cistern VLAN-emitter claim is made.
	conn, err := net.DialTimeout("udp", os.Args[2], 5*time.Second)
	must(err)
	defer conn.Close()
	encoder := sflow.NewEncoder(net.ParseIP("192.0.2.1"), 77, 4294967295)
	encoder.Uptime = 123456
	for i := 0; i < 2; i++ {
		var buf bytes.Buffer
		must(encoder.Encode(&buf, samples))
		must(os.WriteFile(fmt.Sprintf("%s.%d", os.Args[3], i), buf.Bytes(), 0600))
		must(conn.SetWriteDeadline(time.Now().Add(5 * time.Second)))
		n, err := conn.Write(buf.Bytes())
		must(err)
		if n != buf.Len() {
			panic("partial datagram write")
		}
	}
}
