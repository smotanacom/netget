package main

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"github.com/fiorix/go-diameter/v4/diam"
	"github.com/fiorix/go-diameter/v4/diam/avp"
	"github.com/fiorix/go-diameter/v4/diam/datatype"
	"github.com/fiorix/go-diameter/v4/diam/dict"
	"github.com/fiorix/go-diameter/v4/diam/sm"
	"net"
	"os"
	"runtime/debug"
	"sync"
	"time"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}
func text(m *diam.Message, code uint32) string {
	a, e := m.FindAVP(code, 0)
	must(e)
	return string(a.Data.Serialize())
}
func number(m *diam.Message, code uint32) uint32 {
	a, e := m.FindAVP(code, 0)
	must(e)
	switch x := a.Data.(type) {
	case datatype.Unsigned32:
		return uint32(x)
	case datatype.Enumerated:
		return uint32(x)
	default:
		panic("unexpected integer type")
	}
}
func origin(m *diam.Message, host string) {
	_, e := m.NewAVP(avp.OriginHost, avp.Mbit, 0, datatype.DiameterIdentity(host))
	must(e)
	_, e = m.NewAVP(avp.OriginRealm, avp.Mbit, 0, datatype.DiameterIdentity("example"))
	must(e)
}
func reply(c diam.Conn, m *diam.Message, host string) {
	a := m.Answer(2001)
	origin(a, host)
	_, e := a.WriteTo(c)
	must(e)
}
func receive(ch <-chan *diam.Message, request *diam.Message) *diam.Message {
	select {
	case answer := <-ch:
		if answer.Header.HopByHopID != request.Header.HopByHopID || answer.Header.EndToEndID != request.Header.EndToEndID || answer.Header.CommandCode != request.Header.CommandCode {
			panic("uncorrelated answer")
		}
		return answer
	case <-time.After(5 * time.Second):
		panic("answer timeout")
	}
}
func main() {
	mode := flag.String("mode", "client", "server|client|decode")
	addr := flag.String("address", "127.0.0.1:3868", "TCP endpoint")
	password := flag.String("password", "Correct", "test password")
	raw := flag.String("hex", "", "captured frame")
	kind := flag.Uint("request-type", 3, "1..3")
	flag.Parse()
	info, ok := debug.ReadBuildInfo()
	if !ok {
		panic("SDK build info")
	}
	pinned := false
	for _, dep := range info.Deps {
		if dep.Path == "github.com/fiorix/go-diameter/v4" && dep.Version == "v4.5.0" {
			pinned = true
		}
	}
	if !pinned {
		panic("required go-diameter4.5.0")
	}
	if *mode == "decode" {
		b, e := hex.DecodeString(*raw)
		must(e)
		m, e := diam.ReadMessage(bytes.NewReader(b), dict.Default)
		must(e)
		must(m.DecodeErr)
		json.NewEncoder(os.Stdout).Encode(map[string]interface{}{"command": m.Header.CommandCode, "application": m.Header.ApplicationID, "avps": len(m.AVP)})
		return
	}
	host := "client.example"
	if *mode == "server" {
		host = "server.example"
	}
	cfg := &sm.Settings{OriginHost: datatype.DiameterIdentity(host), OriginRealm: "example", VendorID: 0, ProductName: "go-diameter-4.5.0", HostIPAddresses: []datatype.Address{datatype.Address(net.ParseIP("127.0.0.1").To4())}, HandshakeTimeout: 3 * time.Second}
	mux := sm.New(cfg)
	mux.HandleFunc("DPR", func(c diam.Conn, m *diam.Message) {
		if number(m, avp.DisconnectCause) > 2 {
			panic("disconnect cause")
		}
		reply(c, m, host)
	})
	if *mode == "server" {
		mux.HandleFunc("AAR", func(c diam.Conn, m *diam.Message) {
			if m.Header.ApplicationID != 1 || number(m, avp.AuthApplicationID) != 1 || number(m, avp.AuthSessionState) != 1 {
				panic("selected stateless NASREQ fields")
			}
			if len(m.AVP) == 0 || m.AVP[0].Code != avp.SessionID {
				panic("fixed first Session-Id")
			}
			typ := number(m, avp.AuthRequestType)
			if typ < 1 || typ > 3 {
				panic("request type")
			}
			code := uint32(2001)
			if typ != 2 && text(m, avp.UserPassword) != "Correct" {
				code = 4001
			}
			a := m.Answer(code)
			origin(a, host)
			a.InsertAVP(diam.NewAVP(avp.SessionID, avp.Mbit, 0, datatype.UTF8String(text(m, avp.SessionID))))
			a.NewAVP(avp.AuthApplicationID, avp.Mbit, 0, datatype.Unsigned32(1))
			a.NewAVP(avp.AuthRequestType, avp.Mbit, 0, datatype.Enumerated(typ))
			a.NewAVP(avp.AuthSessionState, avp.Mbit, 0, datatype.Enumerated(1))
			a.NewAVP(avp.ReplyMessage, avp.Mbit, 0, datatype.UTF8String("peer verdict"))
			a.NewAVP(avp.ServiceType, avp.Mbit, 0, datatype.Enumerated(1))
			_, e := a.WriteTo(c)
			must(e)
		})
		l, e := net.Listen("tcp", *addr)
		must(e)
		var mu sync.Mutex
		connections := []diam.Conn{}
		srv := &diam.Server{Handler: mux, ReadTimeout: 15 * time.Second, WriteTimeout: 3 * time.Second, OnNewConnection: func(c diam.Conn) { mu.Lock(); connections = append(connections, c); mu.Unlock() }}
		done := make(chan error, 1)
		go func() { done <- srv.Serve(l) }()
		json.NewEncoder(os.Stdout).Encode(map[string]interface{}{"ready": true, "address": l.Addr().String(), "version": "4.5.0", "source_modified": false})
		var line string
		fmt.Scanln(&line)
		must(srv.Close())
		mu.Lock()
		for _, c := range connections {
			c.Close()
		}
		mu.Unlock()
		select {
		case e := <-done:
			if e != diam.ErrServerClosed {
				must(e)
			}
		case <-time.After(3 * time.Second):
			panic("server stop timeout")
		}
		return
	}
	ch := make(chan *diam.Message, 8)
	for _, command := range []string{"AAA", "DWA", "DPA"} {
		mux.HandleFunc(command, func(c diam.Conn, m *diam.Message) { ch <- m })
	}
	cli := &sm.Client{Handler: mux, AuthApplicationID: []*diam.AVP{diam.NewAVP(avp.AuthApplicationID, avp.Mbit, 0, datatype.Unsigned32(1))}, RetransmitInterval: 3 * time.Second}
	c, e := cli.DialTimeout(*addr, 3*time.Second)
	must(e)
	defer c.Close()
	request := diam.NewRequest(265, 1, dict.Default)
	request.Header.CommandFlags |= diam.ProxiableFlag
	origin(request, host)
	request.InsertAVP(diam.NewAVP(avp.SessionID, avp.Mbit, 0, datatype.UTF8String("client.example;calibration;1")))
	request.NewAVP(avp.DestinationRealm, avp.Mbit, 0, datatype.DiameterIdentity("example"))
	request.NewAVP(avp.AuthApplicationID, avp.Mbit, 0, datatype.Unsigned32(1))
	request.NewAVP(avp.AuthRequestType, avp.Mbit, 0, datatype.Enumerated(*kind))
	request.NewAVP(avp.AuthSessionState, avp.Mbit, 0, datatype.Enumerated(1))
	request.NewAVP(avp.UserName, avp.Mbit, 0, datatype.UTF8String("alice"))
	if *kind != 2 {
		request.NewAVP(avp.UserPassword, avp.Mbit, 0, datatype.OctetString(*password))
	}
	_, e = request.WriteTo(c)
	must(e)
	answer := receive(ch, request)
	if text(answer, avp.SessionID) != text(request, avp.SessionID) || number(answer, avp.AuthRequestType) != uint32(*kind) || number(answer, avp.AuthSessionState) != 1 {
		panic("NASREQ answer fields")
	}
	if len(answer.AVP) == 0 || answer.AVP[0].Code != avp.SessionID {
		panic("answer fixed first Session-Id")
	}
	result := number(answer, avp.ResultCode)
	for _, code := range []uint32{280, 282} {
		m := diam.NewRequest(code, 0, dict.Default)
		origin(m, host)
		if code == 282 {
			m.NewAVP(avp.DisconnectCause, avp.Mbit, 0, datatype.Enumerated(0))
		}
		_, e = m.WriteTo(c)
		must(e)
		a := receive(ch, m)
		if number(a, avp.ResultCode) != 2001 {
			panic("base result")
		}
	}
	json.NewEncoder(os.Stdout).Encode(map[string]interface{}{"version": "4.5.0", "source_modified": false, "result_code": result, "request_type": *kind, "stateless": true, "base_commands": []int{257, 280, 282}})
}
