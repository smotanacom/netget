// pfcp-peer: an independent PFCP (3GPP TS 29.244) peer built on wmnsk/go-pfcp, for NetGet's
// tests. Every message it sends is go-pfcp's encoding and every message it receives is parsed
// by go-pfcp; it prints one JSON object per line for the test to read.
//
//	pfcp-peer smf HOST:PORT    drive a UPF through association, heartbeat and a session
//	pfcp-peer upf HOST:PORT    be a UPF: answer whatever an SMF sends, and heartbeat it back
package main

import (
	"encoding/json"
	"fmt"
	"net"
	"os"
	"time"

	"github.com/wmnsk/go-pfcp/ie"
	"github.com/wmnsk/go-pfcp/message"
)

func out(v map[string]any) {
	b, _ := json.Marshal(v)
	fmt.Println(string(b))
}

func fail(err error) {
	out(map[string]any{"error": err.Error()})
	os.Exit(1)
}

func marshal(m message.Message) ([]byte, error) {
	b := make([]byte, m.MarshalLen())
	err := m.MarshalTo(b)
	return b, err
}

func ints(b []byte) []int {
	out := make([]int, len(b))
	for i, x := range b {
		out[i] = int(x)
	}
	return out
}

func cause(i *ie.IE) int {
	if i == nil {
		return -1
	}
	c, err := i.Cause()
	if err != nil {
		return -2
	}
	return int(c)
}

type smf struct {
	conn *net.UDPConn
	seq  uint32
}

// exchange sends a request and returns the parsed response with its raw bytes.
func (s *smf) exchange(m message.Message) (message.Message, []byte) {
	b, err := marshal(m)
	if err != nil {
		fail(err)
	}
	return s.send(b)
}

func (s *smf) send(b []byte) (message.Message, []byte) {
	if _, err := s.conn.Write(b); err != nil {
		fail(err)
	}
	buf := make([]byte, 65535)
	s.conn.SetReadDeadline(time.Now().Add(20 * time.Second))
	n, err := s.conn.Read(buf)
	if err != nil {
		fail(fmt.Errorf("no response: %w", err))
	}
	r, err := message.Parse(buf[:n])
	if err != nil {
		fail(fmt.Errorf("go-pfcp cannot parse the response: %w", err))
	}
	return r, buf[:n]
}

func (s *smf) next() uint32 { s.seq++; return s.seq }

func runSMF(addr string) {
	raddr, err := net.ResolveUDPAddr("udp", addr)
	if err != nil {
		fail(err)
	}
	conn, err := net.DialUDP("udp", nil, raddr)
	if err != nil {
		fail(err)
	}
	s := &smf{conn: conn}
	me := conn.LocalAddr().(*net.UDPAddr).IP.String()
	now := time.Now()
	establish := func(seq uint32) message.Message {
		return message.NewSessionEstablishmentRequest(0, 0, 0, seq, 0,
			ie.NewNodeID(me, "", ""),
			ie.NewFSEID(0x1111, net.ParseIP(me), nil),
			ie.NewCreatePDR(
				ie.NewPDRID(1), ie.NewPrecedence(255),
				ie.NewPDI(
					ie.NewSourceInterface(ie.SrcInterfaceAccess),
					ie.NewFTEID(0x05, 0, nil, nil, 0), // CH|V4: the UPF allocates
					ie.NewNetworkInstance("internet"),
					ie.NewUEIPAddress(0x02, "10.60.0.1", "", 0, 0),
				),
				ie.NewOuterHeaderRemoval(0, 0),
				ie.NewFARID(1),
			),
			ie.NewCreatePDR(
				ie.NewPDRID(2), ie.NewPrecedence(255),
				ie.NewPDI(
					ie.NewSourceInterface(ie.SrcInterfaceCore),
					ie.NewNetworkInstance("internet"),
					ie.NewUEIPAddress(0x06, "10.60.0.1", "", 0, 0),
				),
				ie.NewFARID(2),
			),
			ie.NewCreateFAR(ie.NewFARID(1), ie.NewApplyAction(0x02),
				ie.NewForwardingParameters(ie.NewDestinationInterface(ie.DstInterfaceCore), ie.NewNetworkInstance("internet"))),
			ie.NewCreateFAR(ie.NewFARID(2), ie.NewApplyAction(0x02),
				ie.NewForwardingParameters(ie.NewDestinationInterface(ie.DstInterfaceAccess),
					ie.NewOuterHeaderCreation(0x0100, 0xabcd, "10.0.0.9", "", 0, 0, 0))),
		)
	}

	// A session before any association: the UPF must refuse it (cause 72).
	r, _ := s.exchange(establish(s.next()))
	if m, ok := r.(*message.SessionEstablishmentResponse); ok {
		out(map[string]any{"step": "establish_unassociated", "cause": cause(m.Cause)})
	} else {
		out(map[string]any{"step": "establish_unassociated", "unexpected": r.MessageTypeName()})
	}

	r, _ = s.exchange(message.NewAssociationSetupRequest(s.next(),
		ie.NewNodeID(me, "", ""), ie.NewRecoveryTimeStamp(now), ie.NewCPFunctionFeatures(0x3f, 0x00)))
	m, ok := r.(*message.AssociationSetupResponse)
	if !ok {
		fail(fmt.Errorf("expected an association setup response, got %s", r.MessageTypeName()))
	}
	nodeID, _ := m.NodeID.NodeID()
	_, rtsErr := m.RecoveryTimeStamp.RecoveryTimeStamp()
	out(map[string]any{"step": "associate", "cause": cause(m.Cause), "node_id": nodeID, "recovery_time_stamp": rtsErr == nil, "seq_ok": m.SequenceNumber == s.seq})

	r, _ = s.exchange(message.NewHeartbeatRequest(s.next(), ie.NewRecoveryTimeStamp(now), nil))
	hb, ok := r.(*message.HeartbeatResponse)
	out(map[string]any{"step": "heartbeat", "ok": ok && hb.RecoveryTimeStamp != nil, "seq_ok": ok && hb.SequenceNumber == s.seq})

	r, _ = s.exchange(establish(s.next()))
	es, ok := r.(*message.SessionEstablishmentResponse)
	if !ok {
		fail(fmt.Errorf("expected a session establishment response, got %s", r.MessageTypeName()))
	}
	up, err := es.UPFSEID.FSEID()
	if err != nil {
		fail(fmt.Errorf("no UP F-SEID: %w", err))
	}
	var teids []map[string]any
	for _, c := range es.CreatedPDR {
		id, _ := c.PDRID()
		f, err := c.FTEID()
		if err != nil {
			continue
		}
		teids = append(teids, map[string]any{"pdr_id": id, "teid": f.TEID, "ipv4": f.IPv4Address.String()})
	}
	out(map[string]any{"step": "establish", "cause": cause(es.Cause), "header_seid": es.SEID(), "up_seid": up.SEID, "created_pdr": teids})

	modify := message.NewSessionModificationRequest(0, 0, up.SEID, s.next(), 0,
		ie.NewUpdateFAR(ie.NewFARID(2), ie.NewApplyAction(0x0c))) // BUFF|NOCP: the UE went idle
	mb, err := marshal(modify)
	if err != nil {
		fail(err)
	}
	r, first := s.send(mb)
	mr, ok := r.(*message.SessionModificationResponse)
	out(map[string]any{"step": "modify", "cause": map[bool]int{true: cause(mrCause(mr)), false: -3}[ok], "header_seid": r.SEID()})
	// The same request again (a retransmission): the same answer, byte for byte.
	_, again := s.send(mb)
	out(map[string]any{"step": "retransmit", "identical": string(first) == string(again)})

	r, _ = s.exchange(message.NewSessionDeletionRequest(0, 0, up.SEID, s.next(), 0))
	dr, _ := r.(*message.SessionDeletionResponse)
	out(map[string]any{"step": "delete", "cause": cause(drCause(dr))})
	r, _ = s.exchange(message.NewSessionDeletionRequest(0, 0, up.SEID, s.next(), 0))
	dr, _ = r.(*message.SessionDeletionResponse)
	out(map[string]any{"step": "delete_again", "cause": cause(drCause(dr))})
}

func mrCause(m *message.SessionModificationResponse) *ie.IE {
	if m == nil {
		return nil
	}
	return m.Cause
}

func drCause(m *message.SessionDeletionResponse) *ie.IE {
	if m == nil {
		return nil
	}
	return m.Cause
}

func runUPF(addr string) {
	laddr, err := net.ResolveUDPAddr("udp", addr)
	if err != nil {
		fail(err)
	}
	conn, err := net.ListenUDP("udp", laddr)
	if err != nil {
		fail(err)
	}
	me := conn.LocalAddr().(*net.UDPAddr).IP.String()
	out(map[string]any{"listening": conn.LocalAddr().String()})
	start := time.Now()
	buf := make([]byte, 65535)
	var hbSeq uint32 = 0x700000
	for {
		n, from, err := conn.ReadFromUDP(buf)
		if err != nil {
			fail(err)
		}
		msg, err := message.Parse(buf[:n])
		if err != nil {
			out(map[string]any{"unparseable": err.Error()})
			continue
		}
		seq := msg.Sequence()
		var reply message.Message
		switch m := msg.(type) {
		case *message.AssociationSetupRequest:
			node, _ := m.NodeID.NodeID()
			_, rtsErr := m.RecoveryTimeStamp.RecoveryTimeStamp()
			out(map[string]any{"got": "association_setup", "node_id": node, "recovery_time_stamp": rtsErr == nil})
			reply = message.NewAssociationSetupResponse(seq, ie.NewNodeID(me, "", ""), ie.NewCause(ie.CauseRequestAccepted), ie.NewRecoveryTimeStamp(start))
		case *message.HeartbeatRequest:
			out(map[string]any{"got": "heartbeat"})
			reply = message.NewHeartbeatResponse(seq, ie.NewRecoveryTimeStamp(start))
		case *message.HeartbeatResponse:
			out(map[string]any{"got": "heartbeat_response", "seq_ok": m.SequenceNumber == hbSeq})
			continue
		case *message.SessionEstablishmentRequest:
			cp, err := m.CPFSEID.FSEID()
			if err != nil {
				out(map[string]any{"got": "session_establishment", "error": "no CP F-SEID"})
				continue
			}
			var pdrs []map[string]any
			var created []*ie.IE
			for _, p := range m.CreatePDR {
				id, _ := p.PDRID()
				far, _ := p.FARID()
				src, _ := p.SourceInterface()
				pdrs = append(pdrs, map[string]any{"pdr_id": id, "far_id": far, "source_interface": src})
				pdi, _ := p.PDI()
				for _, x := range pdi {
					if x.Type != ie.FTEID {
						continue
					}
					if f, err := x.FTEID(); err == nil && f.HasCh() {
						created = append(created, ie.NewCreatedPDR(ie.NewPDRID(id), ie.NewFTEID(0x01, 0x100+uint32(id), net.ParseIP(me), nil, 0)))
					}
				}
			}
			var fars []map[string]any
			for _, f := range m.CreateFAR {
				id, _ := f.FARID()
				aa, _ := f.ApplyAction()
				entry := map[string]any{"far_id": id, "apply_action": ints(aa)}
				if fp, err := f.ForwardingParameters(); err == nil {
					for _, x := range fp {
						if x.Type != ie.OuterHeaderCreation {
							continue
						}
						if ohc, err := x.OuterHeaderCreation(); err == nil {
							entry["outer_teid"] = ohc.TEID
							entry["outer_ipv4"] = ohc.IPv4Address.String()
						}
					}
				}
				fars = append(fars, entry)
			}
			node, _ := m.NodeID.NodeID()
			out(map[string]any{"got": "session_establishment", "cp_seid": cp.SEID, "node_id": node, "header_seid": m.SEID(), "pdrs": pdrs, "fars": fars})
			ies := []*ie.IE{ie.NewNodeID(me, "", ""), ie.NewCause(ie.CauseRequestAccepted), ie.NewFSEID(0x9999, net.ParseIP(me), nil)}
			ies = append(ies, created...)
			reply = message.NewSessionEstablishmentResponse(0, 0, cp.SEID, seq, 0, ies...)
			// Then heartbeat the SMF, as a UPF does.
			hbSeq++
			hb, _ := marshal(message.NewHeartbeatRequest(hbSeq, ie.NewRecoveryTimeStamp(start), nil))
			rb, _ := marshal(reply)
			conn.WriteToUDP(rb, from)
			conn.WriteToUDP(hb, from)
			continue
		case *message.SessionModificationRequest:
			var updates []map[string]any
			for _, f := range m.UpdateFAR {
				id, _ := f.FARID()
				aa, _ := f.ApplyAction()
				updates = append(updates, map[string]any{"far_id": id, "apply_action": ints(aa)})
			}
			out(map[string]any{"got": "session_modification", "header_seid": m.SEID(), "update_far": updates})
			reply = message.NewSessionModificationResponse(0, 0, 0x1111, seq, 0, ie.NewCause(ie.CauseRequestAccepted))
		case *message.SessionDeletionRequest:
			out(map[string]any{"got": "session_deletion", "header_seid": m.SEID()})
			reply = message.NewSessionDeletionResponse(0, 0, 0x1111, seq, 0, ie.NewCause(ie.CauseRequestAccepted))
		case *message.AssociationReleaseRequest:
			out(map[string]any{"got": "association_release"})
			reply = message.NewAssociationReleaseResponse(seq, ie.NewNodeID(me, "", ""), ie.NewCause(ie.CauseRequestAccepted))
		default:
			out(map[string]any{"got": msg.MessageTypeName()})
			continue
		}
		rb, err := marshal(reply)
		if err != nil {
			fail(err)
		}
		conn.WriteToUDP(rb, from)
	}
}

func main() {
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: pfcp-peer smf|upf HOST:PORT")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "smf":
		runSMF(os.Args[2])
	case "upf":
		runUPF(os.Args[2])
	default:
		fail(fmt.Errorf("unknown mode %q", os.Args[1]))
	}
}
